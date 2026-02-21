# Contrat XMPP Memoire v1 (Bot + Client)

Ce contrat est concu pour `XEP-0030` (discovery), `XEP-0050` (commandes), `XEP-0004` (formulaires), et `XEP-0059` (pagination optionnelle).

## 1) Namespaces et features

- Namespace metier: `urn:fluux:memory:1`
- Namespace erreurs: `urn:fluux:memory:errors:1`
- Le bot DOIT annoncer via disco#info:
  - `http://jabber.org/protocol/disco#info`
  - `http://jabber.org/protocol/disco#items`
  - `http://jabber.org/protocol/commands`
  - `jabber:x:data`
  - `urn:fluux:memory:1`
  - `http://jabber.org/protocol/rsm` (si pagination activee)

## 2) Modele de donnee canonique

`memory_item`:
- `id` (string, ULID/UUID)
- `owner_jid` (bare JID, derive de l'auth XMPP, jamais accepte depuis le client)
- `agent_id` (string)
- `scope` (`user|shared|session|system`)
- `state` (`active|paused|deleted`)
- `content` (text)
- `tags` (0..n)
- `pinned` (bool)
- `source` (`user_explicit|user_edit|inferred|imported|system`)
- `confidence` (0..1)
- `ttl_seconds` (int, `0` = pas d'expiration)
- `version` (int, optimistic locking)
- `created_at`, `updated_at`, `last_used_at` (ISO-8601 UTC)

## 3) Commandes ad-hoc (nodes)

- `urn:fluux:memory:1:list`
- `urn:fluux:memory:1:get`
- `urn:fluux:memory:1:upsert`
- `urn:fluux:memory:1:set-state`
- `urn:fluux:memory:1:delete`
- `urn:fluux:memory:1:explain`

## 4) Contrat de formulaire XEP-0004

Champs communs:
- `FORM_TYPE` (hidden, valeur = node commande)
- `request_id` (text-single, idempotence)
- `agent_id` (text-single, requis)

`list`:
- `scope` (list-multi)
- `state` (list-multi)
- `tag` (text-multi)
- `query` (text-single)
- `max` (text-single, 1..100, defaut 20)
- `after` (text-single, curseur pagination)

`get`:
- `id` (text-single, requis)

`upsert`:
- `id` (text-single, optionnel; absent => creation)
- `content` (text-multi, requis)
- `scope` (list-single, requis)
- `tag` (text-multi)
- `pinned` (boolean)
- `ttl_seconds` (text-single)
- `source` (list-single)
- `if_version` (text-single, requis si `id` present)
- `reason` (text-single, audit)

`set-state`:
- `id` (text-single, requis)
- `state` (list-single `active|paused`, requis)
- `pinned` (boolean, optionnel)
- `if_version` (text-single, requis)

`delete`:
- `id` (text-single, requis)
- `mode` (list-single `soft|hard`, defaut `soft`)
- `if_version` (text-single, optionnel)
- `reason` (text-single)

`explain`:
- `trace_id` (text-single, requis)

## 5) Payload de resultat (XML metier)

Le bot renvoie dans `<command status='completed'>` un bloc XML `urn:fluux:memory:1`.

Exemple item:

```xml
<mem:item xmlns:mem='urn:fluux:memory:1'
  id='01J...' version='7' scope='user' state='active'
  pinned='true' source='user_edit' confidence='1.0'
  created_at='2026-02-21T10:15:00Z'
  updated_at='2026-02-21T10:16:00Z'
  last_used_at='2026-02-21T10:20:00Z'>
  <mem:agent_id>assistant-main</mem:agent_id>
  <mem:content>Je prefere des reponses courtes.</mem:content>
  <mem:tag>style</mem:tag>
  <mem:tag>preferences</mem:tag>
  <mem:ttl_seconds>0</mem:ttl_seconds>
</mem:item>
```

`list` renvoie `<mem:list total='...'>...</mem:list>` + RSM si active.

## 6) Erreurs normees

Stanza error + extension `urn:fluux:memory:errors:1`:

- Validation: `bad-request` + `<mem:validation field='content' code='required'/>`
- Non autorise: `forbidden` + `<mem:not-authorized/>`
- Introuvable: `item-not-found` + `<mem:not-found id='...'/>`
- Conflit version: `conflict` + `<mem:version-conflict expected='8' got='7'/>`
- Scope invalide: `not-acceptable` + `<mem:invalid-scope/>`
- Limite debit: `resource-constraint` + `<mem:retry-after seconds='30'/>`

## 7) Regles d'autorisation

- `owner_jid` = JID authentifie (source de verite).
- `scope=user|session`: acces uniquement owner.
- `scope=shared`: lecture/ecriture selon role serveur (ACL).
- `scope=system`: lecture seule, jamais modifiable par client.
- Toute mutation DOIT generer un audit event (`who`, `when`, `action`, `reason`, `request_id`).

## 8) Flux minimal d'implementation

Cote client:
1. `disco#info`, puis `disco#items` pour lister les nodes.
2. `commands execute` pour recuperer le formulaire.
3. `commands next/complete` avec `x:data submit`.
4. Gestion stricte des erreurs + retry sur `version-conflict`.

Cote bot:
1. Exposer features + nodes.
2. Valider formulaire par commande.
3. Appliquer ACL avant DB.
4. Optimistic lock via `if_version`.
5. Retourner item canonique apres mutation.

## 9) Exemples de stanzas XMPP

Conventions utilisees dans les exemples:
- Client: `alice@example.com/laptop`
- Bot: `memory-bot.example.com`
- `sessionid` ad-hoc: `sess-001` (exemple)
- Namespace metier: `urn:fluux:memory:1`
- Namespace erreurs: `urn:fluux:memory:errors:1`

### 9.1 Discovery (disco#info et disco#items)

Requete `disco#info`:

```xml
<iq type='get' from='alice@example.com/laptop' to='memory-bot.example.com' id='d1'>
  <query xmlns='http://jabber.org/protocol/disco#info'/>
</iq>
```

Reponse `disco#info`:

```xml
<iq type='result' from='memory-bot.example.com' to='alice@example.com/laptop' id='d1'>
  <query xmlns='http://jabber.org/protocol/disco#info'>
    <identity category='automation' type='bot' name='Fluux Memory Bot'/>
    <feature var='http://jabber.org/protocol/disco#info'/>
    <feature var='http://jabber.org/protocol/disco#items'/>
    <feature var='http://jabber.org/protocol/commands'/>
    <feature var='jabber:x:data'/>
    <feature var='urn:fluux:memory:1'/>
    <feature var='http://jabber.org/protocol/rsm'/>
  </query>
</iq>
```

Requete `disco#items`:

```xml
<iq type='get' from='alice@example.com/laptop' to='memory-bot.example.com' id='d2'>
  <query xmlns='http://jabber.org/protocol/disco#items' node='http://jabber.org/protocol/commands'/>
</iq>
```

Reponse `disco#items` (extrait):

```xml
<iq type='result' from='memory-bot.example.com' to='alice@example.com/laptop' id='d2'>
  <query xmlns='http://jabber.org/protocol/disco#items' node='http://jabber.org/protocol/commands'>
    <item jid='memory-bot.example.com' node='urn:fluux:memory:1:list' name='Memory list'/>
    <item jid='memory-bot.example.com' node='urn:fluux:memory:1:get' name='Memory get'/>
    <item jid='memory-bot.example.com' node='urn:fluux:memory:1:upsert' name='Memory upsert'/>
    <item jid='memory-bot.example.com' node='urn:fluux:memory:1:set-state' name='Memory set state'/>
    <item jid='memory-bot.example.com' node='urn:fluux:memory:1:delete' name='Memory delete'/>
    <item jid='memory-bot.example.com' node='urn:fluux:memory:1:explain' name='Memory explain'/>
  </query>
</iq>
```

### 9.2 Pattern standard ad-hoc (execute -> form -> submit -> completed)

Execute:

```xml
<iq type='set' from='alice@example.com/laptop' to='memory-bot.example.com' id='c1'>
  <command xmlns='http://jabber.org/protocol/commands'
           node='urn:fluux:memory:1:list'
           action='execute'/>
</iq>
```

Le bot repond avec le formulaire (`status='executing'`):

```xml
<iq type='result' from='memory-bot.example.com' to='alice@example.com/laptop' id='c1'>
  <command xmlns='http://jabber.org/protocol/commands'
           node='urn:fluux:memory:1:list'
           sessionid='sess-001'
           status='executing'>
    <x xmlns='jabber:x:data' type='form'>
      <field var='FORM_TYPE' type='hidden'>
        <value>urn:fluux:memory:1:list</value>
      </field>
      <field var='request_id' type='text-single' label='Request id'/>
      <field var='agent_id' type='text-single' label='Agent id' required='true'/>
      <field var='max' type='text-single' label='Max results'/>
    </x>
  </command>
</iq>
```

Le client soumet (`action='complete'`):

```xml
<iq type='set' from='alice@example.com/laptop' to='memory-bot.example.com' id='c2'>
  <command xmlns='http://jabber.org/protocol/commands'
           node='urn:fluux:memory:1:list'
           sessionid='sess-001'
           action='complete'>
    <x xmlns='jabber:x:data' type='submit'>
      <field var='FORM_TYPE'><value>urn:fluux:memory:1:list</value></field>
      <field var='request_id'><value>req-0001</value></field>
      <field var='agent_id'><value>assistant-main</value></field>
      <field var='max'><value>20</value></field>
    </x>
  </command>
</iq>
```

Reponse finale (`status='completed'`):

```xml
<iq type='result' from='memory-bot.example.com' to='alice@example.com/laptop' id='c2'>
  <command xmlns='http://jabber.org/protocol/commands'
           node='urn:fluux:memory:1:list'
           sessionid='sess-001'
           status='completed'>
    <mem:list xmlns:mem='urn:fluux:memory:1' total='1'>
      <mem:item id='01JXYZ' version='7' scope='user' state='active' pinned='true' source='user_edit' confidence='1.0'>
        <mem:agent_id>assistant-main</mem:agent_id>
        <mem:content>Je prefere des reponses courtes.</mem:content>
      </mem:item>
      <set xmlns='http://jabber.org/protocol/rsm'>
        <first>01JXYZ</first>
        <last>01JXYZ</last>
        <count>1</count>
      </set>
    </mem:list>
  </command>
</iq>
```

### 9.3 Commande `list`

Soumission minimale:

```xml
<x xmlns='jabber:x:data' type='submit'>
  <field var='FORM_TYPE'><value>urn:fluux:memory:1:list</value></field>
  <field var='request_id'><value>req-list-1</value></field>
  <field var='agent_id'><value>assistant-main</value></field>
  <field var='scope'><value>user</value><value>shared</value></field>
  <field var='state'><value>active</value></field>
  <field var='query'><value>reponses courtes</value></field>
  <field var='max'><value>10</value></field>
  <field var='after'><value>01JABC</value></field>
</x>
```

### 9.4 Commande `get`

Soumission:

```xml
<x xmlns='jabber:x:data' type='submit'>
  <field var='FORM_TYPE'><value>urn:fluux:memory:1:get</value></field>
  <field var='request_id'><value>req-get-1</value></field>
  <field var='agent_id'><value>assistant-main</value></field>
  <field var='id'><value>01JXYZ</value></field>
</x>
```

Resultat:

```xml
<mem:item xmlns:mem='urn:fluux:memory:1' id='01JXYZ' version='7' scope='user' state='active' pinned='true' source='user_edit' confidence='1.0'>
  <mem:agent_id>assistant-main</mem:agent_id>
  <mem:content>Je prefere des reponses courtes.</mem:content>
  <mem:tag>style</mem:tag>
</mem:item>
```

### 9.5 Commande `upsert`

Creation (pas de `id`):

```xml
<x xmlns='jabber:x:data' type='submit'>
  <field var='FORM_TYPE'><value>urn:fluux:memory:1:upsert</value></field>
  <field var='request_id'><value>req-upsert-create-1</value></field>
  <field var='agent_id'><value>assistant-main</value></field>
  <field var='content'><value>Toujours proposer des exemples concrets.</value></field>
  <field var='scope'><value>user</value></field>
  <field var='tag'><value>style</value><value>preferences</value></field>
  <field var='pinned'><value>1</value></field>
  <field var='ttl_seconds'><value>0</value></field>
  <field var='source'><value>user_explicit</value></field>
  <field var='reason'><value>Preference explicite en chat</value></field>
</x>
```

Mise a jour (avec optimistic lock):

```xml
<x xmlns='jabber:x:data' type='submit'>
  <field var='FORM_TYPE'><value>urn:fluux:memory:1:upsert</value></field>
  <field var='request_id'><value>req-upsert-update-1</value></field>
  <field var='agent_id'><value>assistant-main</value></field>
  <field var='id'><value>01JXYZ</value></field>
  <field var='if_version'><value>7</value></field>
  <field var='content'><value>Je prefere des reponses tres concises.</value></field>
  <field var='scope'><value>user</value></field>
  <field var='reason'><value>Ajustement utilisateur</value></field>
</x>
```

### 9.6 Commande `set-state`

Soumission:

```xml
<x xmlns='jabber:x:data' type='submit'>
  <field var='FORM_TYPE'><value>urn:fluux:memory:1:set-state</value></field>
  <field var='request_id'><value>req-state-1</value></field>
  <field var='agent_id'><value>assistant-main</value></field>
  <field var='id'><value>01JXYZ</value></field>
  <field var='state'><value>paused</value></field>
  <field var='pinned'><value>0</value></field>
  <field var='if_version'><value>8</value></field>
</x>
```

Resultat:

```xml
<mem:item xmlns:mem='urn:fluux:memory:1' id='01JXYZ' version='9' scope='user' state='paused' pinned='false' source='user_edit' confidence='1.0'>
  <mem:agent_id>assistant-main</mem:agent_id>
  <mem:content>Je prefere des reponses tres concises.</mem:content>
</mem:item>
```

### 9.7 Commande `delete`

Soumission (soft delete):

```xml
<x xmlns='jabber:x:data' type='submit'>
  <field var='FORM_TYPE'><value>urn:fluux:memory:1:delete</value></field>
  <field var='request_id'><value>req-del-1</value></field>
  <field var='agent_id'><value>assistant-main</value></field>
  <field var='id'><value>01JXYZ</value></field>
  <field var='mode'><value>soft</value></field>
  <field var='if_version'><value>9</value></field>
  <field var='reason'><value>Preference obsolete</value></field>
</x>
```

Resultat:

```xml
<mem:item xmlns:mem='urn:fluux:memory:1' id='01JXYZ' version='10' scope='user' state='deleted' pinned='false' source='user_edit' confidence='1.0'>
  <mem:agent_id>assistant-main</mem:agent_id>
  <mem:content>Je prefere des reponses tres concises.</mem:content>
</mem:item>
```

### 9.8 Commande `explain`

Soumission:

```xml
<x xmlns='jabber:x:data' type='submit'>
  <field var='FORM_TYPE'><value>urn:fluux:memory:1:explain</value></field>
  <field var='request_id'><value>req-explain-1</value></field>
  <field var='agent_id'><value>assistant-main</value></field>
  <field var='trace_id'><value>trace-7f2a</value></field>
</x>
```

Resultat (exemple):

```xml
<mem:explain xmlns:mem='urn:fluux:memory:1' trace_id='trace-7f2a'>
  <mem:used id='01JXYZ' score='0.91' reason='tag_match:style'/>
  <mem:used id='01JABC' score='0.64' reason='semantic_match'/>
</mem:explain>
```

### 9.9 Erreurs interop (exemples)

Conflit de version:

```xml
<iq type='error' from='memory-bot.example.com' to='alice@example.com/laptop' id='u2'>
  <error type='cancel'>
    <conflict xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/>
    <mem:version-conflict xmlns:mem='urn:fluux:memory:errors:1' expected='10' got='9'/>
  </error>
</iq>
```

Acces refuse:

```xml
<iq type='error' from='memory-bot.example.com' to='alice@example.com/laptop' id='u3'>
  <error type='auth'>
    <forbidden xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/>
    <mem:not-authorized xmlns:mem='urn:fluux:memory:errors:1'/>
  </error>
</iq>
```

## 10) Perspectives: edition des fichiers .md (bot et personas)

Cette section decrit une extension compatible avec ce contrat, pour permettre la modification des definitions Markdown du bot et des futures personas.

### 10.1 Separation claire des domaines

Recommandation:
- Garder la memoire utilisateur dans `urn:fluux:memory:1`.
- Ajouter un namespace separe pour la configuration/persona, par exemple `urn:fluux:persona:1`.
- Ne jamais melanger ACL et schemas entre memoire et persona.

Nodes ad-hoc proposes (persona):
- `urn:fluux:persona:1:list`
- `urn:fluux:persona:1:get`
- `urn:fluux:persona:1:upsert`
- `urn:fluux:persona:1:preview`
- `urn:fluux:persona:1:apply`
- `urn:fluux:persona:1:rollback`

### 10.2 Champ texte long editable

Oui, un champ texte long est possible avec XEP-0004:

```xml
<field var='content' type='text-multi' label='Persona Markdown'/>
```

Contrat recommande pour robustesse:
- `persona.get` renvoie `content`, `version`, `etag` (optionnel), `max_chars`.
- `persona.upsert` recoit `content` + `if_version` (optimistic locking).
- En cas de depassement de taille: erreur `resource-constraint` + `max_chars`.
- Pour tres gros contenus: mode `patch` ou `chunk` plutot qu'un submit unique.

### 10.3 Workflow de changement securise

Workflow conseille:
1. `get` pour recuperer la version courante.
2. `upsert` en brouillon (non actif).
3. `preview` pour obtenir un diff lisible.
4. `apply` pour activer.
5. `rollback` en cas de regression.

Garde-fous:
- Validation schema avant `apply` (sections requises, champs interdits).
- Interdire l'ecriture sur sections systeme verrouillees.
- Audit event sur chaque action (`who`, `when`, `request_id`, `reason`, `old_version`, `new_version`).

### 10.4 ACL et securite

Roles minimaux recommandes:
- `viewer`: lecture uniquement.
- `editor`: edition brouillon + preview.
- `admin`: apply/rollback.
- `owner`: plein controle sur son espace.

Regles:
- Isolation stricte par tenant et par owner.
- `scope=system` toujours read-only.
- Signature/validation serveur sur toute mutation (ne jamais faire confiance au client).

### 10.5 UX client recommandee

Pour rendre l'edition simple cote utilisateur:
- Editeur long texte (Markdown) avec compteur de caracteres.
- Bouton `Preview diff` avant `Apply`.
- Affichage de la version courante et des conflits (`if_version`).
- Message d'erreur actionnable (champ invalide, limite taille, ACL).
- Historique des versions avec bouton `Rollback`.

## 11) References

- [XEP-0030 Service Discovery](https://xmpp.org/extensions/xep-0030.html)
- [XEP-0050 Ad-Hoc Commands](https://xmpp.org/extensions/xep-0050.html)
- [XEP-0004 Data Forms](https://xmpp.org/extensions/xep-0004.html)
- [XEP-0060 Publish-Subscribe](https://xmpp.org/extensions/xep-0060.html)
- [XEP-0059 Result Set Management](https://xmpp.org/extensions/xep-0059.html)
