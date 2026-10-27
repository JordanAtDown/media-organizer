# `/healthz` et `/livez`

Deux routes, pour distinguer deux pannes différentes. Le serveur écoute sur
`health.bind` (défaut `0.0.0.0:8080`) ; il est lié **avant** le premier scan : un port déjà
pris fait échouer le démarrage au lieu de tourner sans supervision.

- **`GET /livez`** → `200 ok`, inconditionnel. Le thread HTTP répond, donc le processus
  n'est pas mort.
- **`GET /healthz`** → JSON. **`503` uniquement si `status` vaut `stale`**, `200` sinon.

## Statuts

| `status` | Signification | Code |
| --- | --- | --- |
| `starting` | aucun scan terminé, mais dans le délai de grâce (ou un premier scan long qui progresse) | 200 |
| `ok` | tout va bien | 200 |
| `degraded` | actif, mais un `input` est absent ou des entrées sont bloquées en transit | 200 |
| `stale` | plus aucun progrès : typiquement un `stat()` bloqué sur un montage NFS figé | **503** |

## Règle de péremption

```text
stale_after   = max(60, poll_interval_secs × stale_intervals)
last_activity = max(fin du dernier scan, dernier fichier traité)
```

C'est le **battement** (`last_progress`, mis à jour à chaque fichier) qui fait la
différence. Un scan légitimement long sur une grosse arborescence continue de battre et
reste `ok` ; une règle fondée seulement sur l'âge du dernier scan *terminé* donnerait un
faux positif. Un scan bloqué sur un montage mort ne bat plus et passe en `stale` en
~90 s, alors que le processus est vivant et que `/livez` répond toujours 200 — exactement le
blocage qu'un watchdog NAS existe pour détecter.

## Exemple de réponse

```json
{
  "status": "ok",
  "version": "1.0.0",
  "uptime_secs": 2,
  "phase": "idle",
  "current_folder": null,
  "scans_completed": 1,
  "last_scan": { "started_at": 1792687600, "finished_at": 1792687600, "duration_secs": 0 },
  "totals": {
    "organized": 2,
    "duplicates": 0,
    "conflicts_skipped": 0,
    "no_date": 0,
    "failed": 0,
    "panics": 0,
    "exif_injected": 1,
    "by_date_source": { "filename": 1, "mtime": 1 }
  },
  "pending_in_staging": 0,
  "caches": { "no_date": 0, "duplicates": 0, "hash_index": 0 },
  "folders": [
    {
      "name": "telephone",
      "input_exists": true,
      "found": 2,
      "organized": 2,
      "duplicates": 0,
      "conflicts_skipped": 0,
      "no_date": 0,
      "failed": 0,
      "panics": 0,
      "exif_injected": 1,
      "by_date_source": { "filename": 1, "mtime": 1 },
      "pending_in_staging": 0,
      "last_error": null
    }
  ]
}
```

## Champs à surveiller

| Champ | Ce qu'il dit |
| --- | --- |
| `phase` / `current_folder` | où en est le scan (`inventory`, `planning`, `resolution`, `preparation`, `commit`, `idle`) |
| `totals.by_date_source` | répartition `exif` / `quicktime` / `filename` / `mtime`. **Un pic de `mtime`** signifie que les fichiers entrants perdent leurs métadonnées en amont |
| `pending_in_staging` | entrées préparées mais non publiées. **Non nul durablement = un commit échoue en boucle** |
| `totals.duplicates` | doublons détectés (laissés en place, sauf `delete_source_duplicates`) |
| `totals.panics` | un fichier a fait paniquer le code ; il est isolé, mais à investiguer |
| `folders[].input_exists` | `false` = montage pas (encore) prêt |
| `folders[].last_error` | dernière erreur de ce dossier |

`phase`, `current_folder`, `last_scan` et `uptime_secs` sont des instantanés ; les `totals`
sont cumulés depuis le démarrage du processus, `folders[]` décrit le **dernier** scan.

## Sonde Docker

`media-organizer healthcheck` interroge `/healthz` en local (une adresse joker est sondée
par la boucle locale) et sort `0` pour tout `2xx`. Il évite d'embarquer `curl` dans l'image
et est branché sur le `HEALTHCHECK` du Dockerfile. Si `health.bind` est vide, il n'y a rien à
sonder et il réussit.
