# media-organizer

Range photos et vidéos par date de prise de vue, **de façon transactionnelle**, depuis un
dossier de dépôt vers une photothèque. Conçu pour tourner en conteneur sur un
Raspberry Pi 5 qui monte un NAS en NFS.

Reprise de `syno-media-organizer` (qui tournait en paquet DSM sur le NAS) : même logique
métier éprouvée, architecture refaite pour le Pi.

```text
<nextcloud>/<email>/files/synchronisation          (dépôt brut, auto-upload du téléphone)
        │  media-organizer : date → nom → doublons → rangement
        ▼
<nextcloud>/<email>/files/photos/phototeque/YYYY/MM/…   (bibliothèque externe Immich)
```

## Ce qu'il fait

1. Parcourt le dossier d'entrée (polling) et **élague** les dossiers parasites (`@eaDir`…).
2. Détermine la date de chaque fichier, dans cet ordre exact :
   **EXIF** `DateTimeOriginal` → **QuickTime** `mvhd` (vidéos) → **nom de fichier**
   (WhatsApp, Android, Facebook, `Resized_`) → **date de dernière modification** en dernier
   recours. Plus aucun fichier ne reste bloqué.
3. Détecte les **doublons** (même nom de destination *et* même contenu) et les laisse en
   place, sans jamais les supprimer.
4. Prépare tout dans une **zone de transit**, puis publie d'un coup par une rafale de
   `rename()` : `output` n'est pas touché avant que tout soit prêt.

Détails et invariants : [docs/PIPELINE.md](docs/PIPELINE.md).

## Démarrage rapide

```bash
cp config.example.toml config.toml      # adapter les chemins
docker run --rm -v "$PWD/config.toml:/etc/media-organizer/config.toml:ro" \
  ghcr.io/jordanatdown/media-organizer:vX.Y.Z validate-config
```

Un exemple de déploiement complet est dans
[docker/docker-compose.example.yml](docker/docker-compose.example.yml). **Toujours
épingler une version** : il n'existe pas de tag `latest`. L'image est **arm64 uniquement**
(Raspberry Pi 5).

**Avant le premier passage en production, lancez `--dry-run`** : il exécute l'inventaire,
la planification et la résolution complets, affiche chaque fichier avec la source de sa
date et sa destination, et n'écrit rien.

```bash
docker run --rm … ghcr.io/jordanatdown/media-organizer:vX.Y.Z --dry-run --once
```

## Configuration

Un fichier TOML, entièrement annoté : [config.example.toml](config.example.toml). Toute
clé inconnue est refusée. Les points à connaître :

- **`file_mode` / `dir_mode`** sont des *chaînes octales* (`"0777"`), pas des entiers.
- **`TZ` est obligatoire** dans le conteneur : sans lui, les fichiers changent de dossier
  (UTC au lieu de l'heure locale). Le fuseau détecté est journalisé au démarrage.
- La **zone de transit** doit être sur le même système de fichiers que `output`.

Priorité : **flag CLI > variable `MO_*` > fichier de config > défaut**.

| Flag | Variable | Rôle |
| --- | --- | --- |
| `-c, --config` | `MO_CONFIG` | fichier de configuration |
| `--state-dir` | `MO_STATE_DIR` | caches persistants |
| `--log-format text\|json` | `MO_LOG_FORMAT` | l'image impose `json` |
| `--log-level`, `-v` | `MO_LOG_LEVEL` | `RUST_LOG` l'emporte |
| `--dry-run` | `MO_DRY_RUN` | planifie sans rien écrire |
| `--once` | `MO_ONCE` | un seul scan puis sortie |
| `--poll-interval-secs` | `MO_POLL_INTERVAL_SECS` | |
| `--workers` | `MO_WORKERS` | |
| `--health-bind` | `MO_HEALTH_BIND` | `""` désactive |

Sous-commandes : `validate-config` (valide et affiche la config effective),
`healthcheck` (sonde `/healthz`, utilisée par le `HEALTHCHECK` Docker), `recover` (rejoue
le journal de transit).

## Supervision

`GET /healthz` (JSON, **503 si plus aucun progrès** — typiquement un montage NFS figé) et
`GET /livez` (200 tant que le processus répond). Format et règles :
[docs/HEALTHZ.md](docs/HEALTHZ.md). Contrat de déploiement :
[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md).

## Développement

```bash
git config core.hooksPath .githooks      # fmt + clippy + tests avant chaque commit
cargo test                               # unitaires + intégration
scripts/smoke-test.sh IMAGE              # bout en bout, en conteneur
```

Commits en anglais, [Conventional Commits](.github/COMMIT_CONVENTION.md), trunk-based
(pas de PR). Publier une version : [RELEASING.md](RELEASING.md).

## Licence

MIT — voir [LICENSE](LICENSE).
