# CLAUDE.md

Organise photos et vidéos par date de prise de vue, de façon transactionnelle, dans un
conteneur arm64 pour Raspberry Pi 5 (NAS monté en NFS). Rust, édition 2021.

**Le guide d'architecture complet est [.claude/CLAUDE.md](.claude/CLAUDE.md)** : lisez-le
avant de toucher au pipeline. Ce fichier n'est qu'un aide-mémoire.

## Cycle quotidien

```bash
git config core.hooksPath .githooks   # une fois : fmt + clippy + tests avant chaque commit
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
scripts/smoke-test.sh IMAGE           # bout en bout, en conteneur
```

## Carte des modules (`src/`)

| Module | Rôle |
| --- | --- |
| `scanner/` | orchestre les 5 phases ; seul à connaître l'ordre (`mod.rs`) — une sous-unité par phase : `inventory`, `planning`, `duplicates`, `dry_run` |
| `plan` | phase 2 — date + destination, **lecture seule** |
| `resolve` | phase 3 — doublons et noms finaux, **en mémoire** |
| `staging` | phases 4-5 — transit, journal, commit, reprise |
| `dedup` / `state` | empreintes BLAKE3 ; les trois caches JSON |
| `date_reader` / `metadata/` | chaîne de date : EXIF → QuickTime → nom → mtime |
| `naming` / `fsops` | motif de destination ; primitives (modes, lien-ou-copie, `write_atomic`) |
| `config` / `cli` | schéma TOML + validation ; flags et variables `MO_*` |
| `notify/` | annonce de fin de lot : événement fichier atomique, `seq` persistant, webhook |
| `http` | client HTTP minimal (sonde `healthcheck`, webhook), sans dépendance ni TLS |
| `health/` / `shutdown` / `daemon` | `/healthz`, signaux, boucle de scan |

## Règles

- Documentation en **français** ; messages de commit et commentaires de code en **anglais**.
- Conventional Commits (imposés par `.githooks/commit-msg`), trunk-based, un commit = une
  intention, pas de branche de feature.
- **Ne jamais committer ni pousser sans demande explicite.**
- Jamais de secret dans un fichier commité.
- Détails, invariants et pièges : [.claude/CLAUDE.md](.claude/CLAUDE.md). Procédures
  outillées : `.claude/skills/` (`release`, `verify-container`, `add-date-source`).
