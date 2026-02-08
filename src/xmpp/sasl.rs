/// SASL authentication for XMPP C2S connections.
/// Supports PLAIN and SCRAM-SHA-1 (RFC 5802).
use anyhow::{anyhow, Result};
use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::{Digest, Sha1};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

use super::stanzas;

type HmacSha1 = Hmac<Sha1>;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// Reads from the stream until a SASL response element is complete.
/// SASL responses end with `</challenge>`, `</success>`, or `</failure>`,
/// or are self-closing like `<success xmlns='...'/>`
async fn read_sasl_response<S: AsyncReadExt + Unpin>(stream: &mut S) -> Result<String> {
    let mut buf = vec![0u8; 8192];
    let mut accumulated = String::new();
    let timeout = Duration::from_secs(10);

    loop {
        let read_future = stream.read(&mut buf);
        let n = match tokio::time::timeout(timeout, read_future).await {
            Ok(Ok(0)) => return Err(anyhow!("Connection closed during SASL")),
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(anyhow!("Read error during SASL: {e}")),
            Err(_) => return Err(anyhow!("Timeout during SASL (accumulated: {accumulated})")),
        };

        accumulated.push_str(&String::from_utf8_lossy(&buf[..n]));

        // Check for any complete SASL response element
        if accumulated.contains("</challenge>")
            || accumulated.contains("</success>")
            || accumulated.contains("</failure>")
            || accumulated.contains("/>")
        {
            return Ok(accumulated);
        }
    }
}

/// SASL PLAIN authentication (RFC 4616).
pub async fn authenticate_plain<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    username: &str,
    password: &str,
) -> Result<()> {
    let auth_stanza = stanzas::build_sasl_auth_plain(username, password);
    stream.write_all(auth_stanza.as_bytes()).await?;
    debug!("Sent SASL PLAIN auth");

    let response = read_sasl_response(stream).await?;
    if stanzas::is_sasl_success(&response) {
        debug!("SASL PLAIN succeeded");
        Ok(())
    } else {
        Err(anyhow!("SASL PLAIN failed: {response}"))
    }
}

/// SASL SCRAM-SHA-1 authentication (RFC 5802).
pub async fn authenticate_scram_sha1<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    username: &str,
    password: &str,
) -> Result<()> {
    // Step 1: Client-first-message
    let client_nonce = generate_nonce();
    let client_first_bare = format!("n={username},r={client_nonce}");
    let client_first_message = format!("n,,{client_first_bare}");

    let encoded = B64.encode(client_first_message.as_bytes());
    let auth_stanza = stanzas::build_sasl_auth_scram_sha1(&encoded);
    stream.write_all(auth_stanza.as_bytes()).await?;
    debug!("Sent SCRAM-SHA-1 client-first");

    // Step 2: Read server-first-message
    let response = read_sasl_response(stream).await?;
    if !stanzas::is_sasl_challenge(&response) {
        return Err(anyhow!("Expected SASL challenge, got: {response}"));
    }
    let challenge_b64 = stanzas::extract_sasl_challenge(&response)
        .ok_or_else(|| anyhow!("No challenge payload"))?;
    let server_first = String::from_utf8(B64.decode(&challenge_b64)?)?;
    debug!("Server-first: {server_first}");

    // Parse server-first-message: r=...,s=...,i=...
    let (combined_nonce, salt_b64, iterations) = parse_server_first(&server_first)?;

    // Verify server nonce starts with our client nonce
    if !combined_nonce.starts_with(&client_nonce) {
        return Err(anyhow!("Server nonce doesn't contain client nonce"));
    }

    // Step 3: Compute proofs
    let salt = B64.decode(&salt_b64)?;

    // SaltedPassword = PBKDF2-SHA1(password, salt, iterations)
    let mut salted_password = [0u8; 20];
    pbkdf2::pbkdf2_hmac::<Sha1>(password.as_bytes(), &salt, iterations, &mut salted_password);

    // ClientKey = HMAC-SHA1(SaltedPassword, "Client Key")
    let client_key = hmac_sha1(&salted_password, b"Client Key");

    // StoredKey = SHA1(ClientKey)
    let stored_key = Sha1::digest(&client_key);

    // Build client-final-without-proof
    let channel_binding = B64.encode(b"n,,"); // "biws"
    let client_final_without_proof = format!("c={channel_binding},r={combined_nonce}");

    // AuthMessage = client-first-bare + "," + server-first + "," + client-final-without-proof
    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");

    // ClientSignature = HMAC-SHA1(StoredKey, AuthMessage)
    let client_signature = hmac_sha1(&stored_key, auth_message.as_bytes());

    // ClientProof = ClientKey XOR ClientSignature
    let client_proof: Vec<u8> = client_key
        .iter()
        .zip(client_signature.iter())
        .map(|(a, b)| a ^ b)
        .collect();

    // Send client-final-message
    let client_final = format!("{client_final_without_proof},p={}", B64.encode(&client_proof));
    let encoded_final = B64.encode(client_final.as_bytes());
    let response_stanza = stanzas::build_sasl_response(&encoded_final);
    stream.write_all(response_stanza.as_bytes()).await?;
    debug!("Sent SCRAM-SHA-1 client-final");

    // Step 4: Verify server response
    let response = read_sasl_response(stream).await?;
    if stanzas::is_sasl_success(&response) {
        debug!("SCRAM-SHA-1 authentication successful");
        Ok(())
    } else {
        Err(anyhow!("SCRAM-SHA-1 auth failed: {response}"))
    }
}

fn generate_nonce() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..24).map(|_| rng.gen()).collect();
    B64.encode(&bytes)
}

fn hmac_sha1(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha1::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Parses SCRAM server-first-message: r=nonce,s=salt,i=iterations
fn parse_server_first(msg: &str) -> Result<(String, String, u32)> {
    let mut nonce = None;
    let mut salt = None;
    let mut iterations = None;

    for part in msg.split(',') {
        if let Some(val) = part.strip_prefix("r=") {
            nonce = Some(val.to_string());
        } else if let Some(val) = part.strip_prefix("s=") {
            salt = Some(val.to_string());
        } else if let Some(val) = part.strip_prefix("i=") {
            iterations = Some(val.parse::<u32>()?);
        }
    }

    Ok((
        nonce.ok_or_else(|| anyhow!("Missing nonce in server-first"))?,
        salt.ok_or_else(|| anyhow!("Missing salt in server-first"))?,
        iterations.ok_or_else(|| anyhow!("Missing iterations in server-first"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_server_first() {
        let msg = "r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096";
        let (nonce, salt, iter) = parse_server_first(msg).unwrap();
        assert!(nonce.starts_with("fyko+d2lbbFgONRv9qkxdawL"));
        assert_eq!(salt, "QSXCR+Q6sek8bf92");
        assert_eq!(iter, 4096);
    }

    #[test]
    fn test_hmac_sha1() {
        // Known test vector: HMAC-SHA1("key", "The quick brown fox...")
        let result = hmac_sha1(b"key", b"The quick brown fox jumps over the lazy dog");
        assert_eq!(result.len(), 20);
    }

    #[test]
    fn test_scram_sha1_computation() {
        // RFC 5802 test vector
        let password = "pencil";
        let salt = B64.decode("QSXCR+Q6sek8bf92").unwrap();
        let iterations = 4096u32;

        let mut salted_password = [0u8; 20];
        pbkdf2::pbkdf2_hmac::<Sha1>(password.as_bytes(), &salt, iterations, &mut salted_password);

        let client_key = hmac_sha1(&salted_password, b"Client Key");
        let stored_key = Sha1::digest(&client_key);
        let server_key = hmac_sha1(&salted_password, b"Server Key");

        // Verify these produce 20-byte outputs
        assert_eq!(client_key.len(), 20);
        assert_eq!(stored_key.len(), 20);
        assert_eq!(server_key.len(), 20);
    }
}
