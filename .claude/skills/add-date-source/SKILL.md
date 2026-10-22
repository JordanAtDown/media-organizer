---
name: add-date-source
description: Add support for a new capture-date source in media-organizer — a new filename pattern (e.g. another phone/app naming scheme) or a new metadata container. Use whenever the user wants files named a certain way to be dated correctly, reports files landing in the wrong month, or asks to support a new format.
---

# Ajouter une source de date

La chaîne est **EXIF → QuickTime → nom de fichier → mtime**, dans cet ordre exact
([src/date_reader.rs](../../../src/date_reader.rs)). Ajouter une source, c'est ajouter un
lecteur derrière le trait `DateReader`, pas refactorer.

## Règle d'or

**Ne jamais changer la date calculée pour un format déjà pris en charge.** La photothèque
est déjà rangée ; un décalage d'une heure (ou changer de source) déplacerait des fichiers
existants et fracturerait un mois. Une nouvelle source ne doit s'appliquer qu'à des fichiers
qui n'avaient **aucune** date jusque-là (donc qui tombaient sur le mtime).

Ne jamais faire passer le mtime avant une source plus fiable.

## Nouveau motif de nom de fichier

1. Dans [src/metadata/filename.rs](../../../src/metadata/filename.rs), écrire
   `parse_<nom>_date(stem: &str) -> Option<DateTime<Local>>` :
   - **jamais de découpage par indices d'octets** (`&s[..8]` panique sur un nom non ASCII) :
     utiliser `str::get` ;
   - valider les chiffres et laisser `Local.with_ymd_and_hms(..).single()` rejeter les dates
     impossibles ;
   - si le nom ne porte que le jour, fixer l'heure à midi (comme WhatsApp).
2. L'ajouter à la chaîne `or_else` de `parse_date_from_filename`, **après** les motifs
   existants (l'ordre décide en cas d'ambiguïté).
3. Tests paramétrés `rstest` dans le même fichier : cas valides, cas rejetés (date
   impossible, préfixe voisin), et non-ASCII qui ne doit pas paniquer.
4. Un test bout en bout dans `tests/test_filename_date.rs` (cas `#[case]` de plus).

## Nouveau conteneur de métadonnées

1. Un module sous `src/metadata/` qui lit **le minimum** (comme `isobmff` ne va que de
   `moov` à `mvhd`) et renvoie `Result<DateTime<Local>, ExifError>` ou `None`.
2. Implémenter `DateReader` dans [src/date_reader.rs](../../../src/date_reader.rs) et l'ajouter
   à `fallback_readers()`, ou, pour une source primaire, l'ajouter à `read_capture`.
3. Étendre `DateSource` (variante sérialisée en `snake_case`) et son comptage dans
   `FolderSummary::count_source` (`src/scanner.rs`) pour que `/healthz` la sépare.
4. Un échec de lecture doit **dégrader vers la source suivante, jamais vers une date fausse**.
5. Une fixture binaire n'est admise que comme extrait de métadonnées < ~10 Ko, avec
   provenance et valeur attendue dans `tests/fixtures/README.md`.

## Vérifier

```bash
cargo test
cargo run -- -c config.toml --dry-run --once   # montre la source de date de chaque fichier
```

Mettre à jour `docs/PIPELINE.md` (table de la chaîne) et `README.md` si une source est ajoutée.
