# Fixtures de test

Les fichiers média sont **générés à l'exécution** (`tests/common/mod.rs`). La seule
exception admise : un extrait de métadonnées d'un vrai appareil, quand la structure
exacte ne peut pas être reproduite fidèlement par des données synthétiques.

Règle : une fixture binaire est admise uniquement comme **extrait de métadonnées sans
charge média**, sous ~10 Ko, avec provenance, commande d'extraction et valeur attendue
documentées ici.

| Fichier | Appareil | Taille | Date de création attendue (UTC) |
| --- | --- | --- | --- |
| `sample_iphone.mov` | iPhone XS (iOS 18.7.4) | 4,7 Ko | `2026-02-16 12:37:05` |

## Pourquoi celle-ci

Les `.mov` Apple contiennent des boîtes `tapt`, `clef`, `prof`, `enof` qui font échouer
*silencieusement* les parseurs d'arbre complet. Le lecteur `metadata::isobmff` ne va que
de `moov` à `mvhd` précisément pour y résister ; cette fixture est le garde-fou.

## Provenance

Extrait repris du dépôt `syno-media-organizer` (`tests/fixtures/sample_iphone.mov`),
lui-même un extrait d'en-tête (`ftyp` + `moov`, sans flux vidéo ni audio).

```bash
# Extraire ftyp + moov seulement ("Media Data Offset" d'exiftool = limite en octets) :
head -c <media_data_offset> <source.mov> > tests/fixtures/<nom>.mov

# Vérifier la date attendue :
exiftool -CreateDate tests/fixtures/<nom>.mov
```
