# Contrat de déploiement

Ce que doit savoir quiconque déploie l'image — en particulier la future tâche Ansible du
dépôt d'infrastructure. Ce dépôt-ci ne déploie rien lui-même.

## Image

- `ghcr.io/jordanatdown/media-organizer:vX.Y.Z`, **publique**, tirable sans authentification.
- **`linux/arm64` uniquement** (Raspberry Pi 5). Pas de tag `latest`, pas de tag flottant :
  épingler la version exacte.
- Binaire = `PID 1` (via `exec`), logs sur **stdout** (JSON par défaut), arrêt propre sur
  SIGTERM (code de sortie `0`, en moins d'une seconde même pendant l'attente entre scans).

## Variables d'environnement

| Variable | Obligatoire | Rôle |
| --- | --- | --- |
| `TZ` | **oui** | fuseau horaire. Sans lui les dates sont en UTC et les fichiers changent de dossier au passage de mois. Le fuseau détecté est journalisé au démarrage (`timezone_offset`). Les horodatages des logs suivent le même fuseau (`2026-10-22 20:25:42`, heure locale, JSON comme texte) ; sans `TZ` ils sont en UTC |
| `PUID` / `PGID` | recommandé (défaut 1000) | uid/gid sous lesquels le processus tourne : **c'est l'identité vue par le NAS** |
| `MO_CONFIG` | non | défaut `/etc/media-organizer/config.toml` |
| `MO_STATE_DIR` | non | défaut `/var/lib/media-organizer` |
| `MO_LOG_FORMAT`, `MO_LOG_LEVEL`, `MO_DRY_RUN`, `MO_ONCE`, `MO_POLL_INTERVAL_SECS`, `MO_WORKERS`, `MO_HEALTH_BIND` | non | voir le [README](../README.md) |

Chaque ligne JSON est **à plat** : `{"timestamp":"2026-10-22 20:25:42","level":"ERROR","message":"commit failed; will retry","path":"/inputs/jordan/IMG_2894.JPG","error":"…"}` (`message` et les propriétés au premier niveau, pas de `fields.` ni de `target`).

## Volumes

Trois catégories de dossiers, déclarées **une par une** (voir le README) :

| Montage | Mode | Contenu |
| --- | --- | --- |
| `/etc/media-organizer/config.toml` | `ro` | configuration (templatable : clés inconnues = erreur) |
| `/var/lib/media-organizer` | `rw` | caches persistants **et zone de travail** (`work/`), et avec `[notify]` le compteur `notify.json` et le dossier d'événements (`events/`, lisible côté hôte). Volume distinct de la configuration |
| `/inputs/<nom>` | `rw` | un dossier **source** (synchronisation), un volume par dossier |
| `/libraries/<nom>` | `rw` | un dossier qui **accueille une librairie**, un volume par dossier |

`config.toml` relie un input à une library par leur nom (`input = "jordan"`,
`library = "jordan"`). Les racines `/inputs` et `/libraries` se règlent avec `inputs_root` et
`libraries_root` ; des chemins absolus restent possibles.

**Montages séparés = copie, pas lien physique.** Un `hard_link` est impossible entre deux
montages (`EXDEV`), même s'ils viennent du même export NFS : chaque fichier est copié (lu sur
l'input, écrit sur la librairie, via le Pi). Ce n'est pas un problème pour un flux normal ;
un gros arriéré au premier passage est plus long, borné par `[staging] max_batch`.

**La librairie ne reçoit que le résultat.** Tout le travail se fait dans la zone de travail
privée ; la librairie reçoit des copies sous un nom caché `.<nom>.mo-part` (que les scanners
de librairie comme Immich n'indexent pas : ils ne retiennent que les noms qui *finissent* par
une extension média), puis les fichiers prennent leur vrai nom d'un coup, par un lien
atomique qui n'écrase jamais un fichier existant. Après une coupure franche, d'éventuels
`.mo-part` orphelins sont repris ou purgés au scan suivant.

La zone de travail n'a pas à être montée. Pour la déboguer, montez `./work` sur
`/var/lib/media-organizer/work`.

L'entrypoint ne fait un `chown` **que** sur le dossier d'état — jamais sur les volumes
média (ce serait une réécriture récursive de tout le NAS via NFS). Les uid/gid `PUID`/`PGID`
doivent avoir les droits d'écriture sur chaque librairie et d'écriture/suppression sur chaque
input (la source est supprimée après publication).

**Modes des dossiers (`dir_mode`).** Un `chmod` n'est permis qu'au propriétaire, même quand
le mode demandé est déjà le mode actuel. Or les dossiers historiques d'une librairie
appartiennent souvent à d'autres uid ; seuls ceux que le service crée lui-même sont à lui.
Pour chaque niveau, de la destination jusqu'à la racine de la librairie, le service :

- **ne touche à rien** si le mode actuel est déjà le mode configuré ;
- applique `chmod` si le mode diffère ;
- **tolère un refus** (`EPERM`) : il journalise un `warn` (chemin, mode actuel, mode voulu)
  et poursuit, le dossier restant utilisable dans le cas courant. Toute autre erreur fait
  échouer le fichier, qui est retenté au scan suivant.

Un `warn` répété sur le même dossier signale un mode réellement insuffisant (ex. un dossier
en `0755` d'un autre uid) : corrigez-le côté NAS, le service ne le pourra pas.

## Réseau et supervision

Port `8080` (`health.bind`). Voir [HEALTHZ.md](HEALTHZ.md) pour le détail.

- Probe de supervision (ex. Gatus) : `GET http://<hôte>:8080/healthz`, condition sur le
  code HTTP (`200` = vivant, `503` = bloqué). Pour aussi alerter sur `degraded`, comparer
  `status` à `ok` dans le corps.
- Healthcheck Docker intégré : `media-organizer healthcheck` (intervalle 30 s, démarrage
  toléré 60 s).
- Logo Dozzle : l'image porte le label `dev.dozzle.icon` (SVG en data URI, source dans
  `docker/dozzle-icon.svg`, 16 Ko max). Pour un autre logo, redéfinir le label côté
  `docker-compose` (`dev.dozzle.icon: <nom dashboard-icons>` ou data URI) ; `none` le masque.
- Le conteneur monte des bind-mounts NFS : il a sa place dans la liste des conteneurs
  qu'un watchdog doit relancer quand le montage revient.

## Annonce de fin de lot (`[notify]`)

Nextcloud ne voit pas les écritures de media-organizer : un `occ files:scan` est nécessaire.
Le conteneur n'a ni Docker ni `occ` et reste indépendant de Nextcloud : il **annonce** la fin
d'un lot, et c'est à l'infrastructure de réagir. **Désactivé par défaut** : sans
`[notify]`, rien n'est écrit et rien ne change pour les autres déploiements.

```toml
[notify]
dir = "/var/lib/media-organizer/events"                    # fichier d'événement (durable)
# webhook_url = "http://host.docker.internal:8123/batch"   # en plus, facultatif
# quiet_secs = 60       # annonce quand l'input est resté calme 60 s…
# max_wait_secs = 600   # …mais jamais plus de 10 min après le premier fichier retenu
```

### Quand un événement est publié

Pour chaque **dossier**, quand ce qu'un scan a rangé a été **retenu** puis que l'input est
resté calme (voir ci-dessous). Il faut qu'au moins un fichier ait été publié dans la
librairie ou une source supprimée de l'input. Jamais :

- en `--dry-run` ;
- pour un lot qui ne change rien (doublons conservés, fichiers en erreur, rien de neuf).

La sous-commande `recover` et le rejeu d'un journal au démarrage annoncent aussi ce qu'ils
publient, y compris un fichier publié juste avant une coupure (`recover` publie
immédiatement : il s'arrête ensuite et aucun scan ne viendrait vider ce qui serait retenu).

**Pourquoi attendre : les uploads arrivent au fil de l'eau.** Le dossier d'entrée est
alimenté par des uploads dont les fichiers n'apparaissent pas ensemble ; media-organizer les
range dès qu'ils sont stables (`stability_secs`), donc le lot d'un scan est rarement tout
l'upload. Annoncer chaque scan ferait relancer un `files:scan` toutes les 30 secondes. Seule
**l'annonce** attend : le rangement, lui, n'est jamais retardé. Ce qui a changé est retenu et
cumulé (les fichiers, mais aussi les dossiers touchés), puis publié en **un seul événement**
dès que :

- plus aucun fichier n'a été rangé depuis `quiet_secs` (défaut **60 s**, compté depuis le
  *dernier* fichier), ou
- `max_wait_secs` (défaut **600 s**) se sont écoulées depuis le *premier* fichier retenu.

Le plafond est ce qui évite le défaut d'un débounce naïf, un flux continu qui repousse
l'annonce indéfiniment : même sous un flux ininterrompu, un événement part au plus toutes les
`max_wait_secs`. `quiet_secs = 0` annonce après chaque lot, sans attendre. La décision est
prise à chaque scan : le délai réel est arrondi au `poll_interval_secs` supérieur.

Ce qui est retenu est **persisté** (`state_dir/notify.json`) : un redémarrage ne l'oublie pas
et l'annonce part au premier scan où elle est due. Tant qu'elle est retenue, `seq` ne bouge
pas et `/healthz` ne change pas. Un grand arriéré est borné par `staging.max_batch` : chaque
scan en range une part, et le tout est annoncé d'un bloc une fois l'input calme (ou au
plafond).

### Le fichier

`<dir>/<nom du dossier>.json` (le nom est le `name` du `[[folders]]`, assaini), écrasé à
chaque lot :

```json
{"folder":"jordan","seq":42,"finished_at":"2026-10-27T23:07:11+01:00",
 "committed":12,"deleted_sources":3,"library_dirs":["2026/10"],"input_dirs":["."]}
```

| Champ | Sens |
| --- | --- |
| `seq` | croît de 1 à chaque lot **de ce dossier**, et survit aux redémarrages (`state_dir/notify.json`). Un trou ou un retour en arrière = un événement manqué |
| `finished_at` | RFC 3339, heure locale (`TZ`), à la seconde : le moment de la publication |
| `committed` | fichiers publiés dans la librairie **depuis l'événement précédent** (tous les lots que l'annonce a traversés) |
| `deleted_sources` | sources de doublons supprimées de l'input (`dedup.delete_source_duplicates`), idem |
| `library_dirs` | dossiers de la librairie touchés, **relatifs à sa racine** (`"."` = la racine) |
| `input_dirs` | dossiers de l'input d'où des fichiers ont disparu, **relatifs à l'input**. Tout fichier commité quitte aussi l'input : ils y figurent, avec les doublons supprimés (`committed` ne compte pas les secondes) |

Les chemins sont triés et sans doublon, pour viser un scan restreint
(`…/phototeque/2026/10`) plutôt que toute la librairie. Lister un dossier de trop est sans
conséquence (un scan est idempotent) ; en omettre un serait un bug. Un chemin qui ne peut
pas s'exprimer relativement devient `"."`.

**Lecture concurrente sûre.** Le JSON est écrit dans `<dir>/.tmp/`, synchronisé sur disque,
puis déplacé par `rename` : un lecteur voit l'ancien fichier ou le nouveau complet, jamais un
fichier tronqué. Le temporaire n'est volontairement **pas** dans `<dir>` : un `.path` systemd
qui surveille ce dossier ne se déclenche qu'au `rename`, une seule fois, contenu complet. Le
fichier est en `0644`.

**Règles pour le consommateur.** L'événement suivant écrase le précédent : si deux lots
arrivent avant une lecture, les `library_dirs` du premier sont perdus.

1. mémoriser le dernier `seq` traité, par dossier ;
2. `seq == dernier + 1` : rien de manqué, scanner `library_dirs` et `input_dirs` ;
3. `seq > dernier + 1` (trou) ou `seq < dernier` (compteur perdu) : **scanner tout le
   dossier** ;
4. relire les événements jusqu'à ce qu'aucun `seq` ne change, au cas où un lot arrive
   pendant le scan.

Une annonce peut se perdre dans une fenêtre très étroite : un arrêt brutal (`kill -9`,
coupure) entre le moment où l'annonce part et l'écriture de l'événement ; le `seq` est alors
consommé et le consommateur voit le trou. Ce qui est *retenu* ne se perd pas (il est
persisté). Un scan complet périodique côté infrastructure sert de filet.

### Un déclencheur systemd, sans démon

Le fichier suffit à remplacer un `inotifywait` : le déclencheur est durable même si le
consommateur est arrêté un moment, sans port ouvert ni processus à superviser. Exemple
indicatif (le dossier hôte est celui du volume d'état, `./state/events`) :

```ini
# /etc/systemd/system/mo-nextcloud-scan.path
[Path]
PathChanged=/srv/media-organizer/state/events
Unit=mo-nextcloud-scan.service

[Install]
WantedBy=multi-user.target
```

```ini
# /etc/systemd/system/mo-nextcloud-scan.service
[Service]
Type=oneshot
ExecStart=/usr/local/bin/mo-nextcloud-scan   # applique les règles ci-dessus, puis occ files:scan
```

Le lien dossier → chemin Nextcloud (`<email>/files/photos/<dossier>` pour la librairie,
`<email>/files/synchronisation/…` pour l'input) appartient à l'infrastructure. À valider
côté Nextcloud : un `files:scan --path` sur un dossier **nouveau** (le premier fichier d'un
nouveau mois) l'indexe-t-il bien, ou faut-il remonter au parent ?

### Le webhook

`webhook_url` envoie le **même JSON** en `POST` (`Content-Type: application/json`). Limites
assumées :

- **`http://` uniquement** : l'image n'embarque pas de pile TLS (et `https://` est refusé à la
  validation de la configuration). Depuis le conteneur, l'hôte s'atteint avec
  `extra_hosts: ["host.docker.internal:host-gateway"]` ;
- **un seul essai**, sans nouvelle tentative : un récepteur arrêté **perd l'événement**. Le
  fichier est le chemin durable, le webhook un complément ;
- **l'envoi est synchrone** : media-organizer attend la réponse avant de poursuivre son scan.
  Tout l'échange (résolution du nom, connexion, envoi, lecture de la réponse) est borné à
  **5 secondes au total**. Un récepteur arrêté répond aussitôt par un refus de connexion ; un
  hôte injoignable, un DNS muet ou un récepteur qui répond au compte-gouttes coûtent 5 s au
  plus, une fois par événement et par dossier (et les événements sont espacés d'au moins
  `quiet_secs`) ;
- tout code `2xx` vaut livraison ; le corps de la réponse est ignoré.

### Écrire un récepteur

La requête est un `POST` sur le chemin de `webhook_url`, avec `Content-Type: application/json`
et `User-Agent: media-organizer/<version>`. Le corps est **le même JSON que le fichier**
(schéma ci-dessus), sur une seule ligne. Ce que doit faire le récepteur :

1. **Répondre `2xx` tout de suite** (`204` convient), sans attendre la fin du traitement.
   media-organizer est bloqué tant qu'il n'a pas la réponse, et sans réponse complète en 5 s
   au total l'événement est compté en échec (`degraded` dans `/healthz`) même si le récepteur l'a bien
   reçu. Un `files:scan` long se lance **après** la réponse : mettre l'événement en file ou
   démarrer un traitement séparé.
2. **Ne pas compter sur la livraison.** Il n'y a ni nouvelle tentative ni accusé persistant.
   Appliquer les mêmes règles que pour le fichier : mémoriser le dernier `seq` par `folder` ;
   `seq == dernier + 1`, scanner `library_dirs` et `input_dirs` ; **trou** (`seq > dernier + 1`)
   **ou retour en arrière** (`seq < dernier`, compteur perdu), scanner tout le dossier.
   À la différence du fichier, chaque événement est livré séparément, rien n'est écrasé : si le
   récepteur est joignable, les `library_dirs` de chaque événement lui parviennent.
3. **Traiter les événements un par un, dans l'ordre d'arrivée.** L'envoi étant synchrone, deux
   événements d'un même dossier ne se chevauchent jamais, et ceux de dossiers différents
   arrivent l'un après l'autre.
4. **Tolérer un événement vu deux fois** : relancer un scan sur les mêmes dossiers est sans
   effet de bord.
5. **Rester sur un réseau de confiance.** Il n'y a ni authentification, ni secret, ni signature,
   et `https://` n'est pas disponible : quiconque joint l'URL peut forger un événement. L'effet
   se limite à déclencher un scan, mais n'exposez pas le récepteur au-delà de l'hôte (écoutez
   sur la boucle locale ou sur l'interface du pont Docker, pas sur `0.0.0.0` côté réseau).

Une absence d'événement n'est pas une panne : rien n'est envoyé quand rien n'a été rangé, ni
tant qu'une annonce est retenue par le débounce (`quiet_secs`).

### Erreurs et supervision

Dossier absent, non inscriptible, récepteur injoignable : le lot se déroule normalement et un
`warn` est journalisé (`cannot announce the batch`, avec `sink` = `event file` ou
`webhook`). Un dossier manquant est créé ; un `dir` inscriptible est vérifié au démarrage
(`warn` si non). Les deux sorties sont indépendantes : l'une qui échoue n'empêche pas
l'autre. Le `seq` est consommé même en cas d'échec : le consommateur verra le trou.

[`/healthz`](HEALTHZ.md) expose, par dossier, le dernier `seq`, sa date et `last_error`, et
passe en `degraded` tant que la dernière annonce a échoué. **Attention à Gatus :** « un lot
a été annoncé récemment » alerterait à tort les jours sans photo (et, avec le débounce, un
événement ne suit le dernier fichier que d'au moins `quiet_secs`). Les signaux utiles sont
`last_error` nul, et côté consommateur le `seq` traité comparé à celui de `/healthz`.

## Première mise en service

1. Valider la configuration sans rien démarrer :

   ```bash
   docker run --rm -v ./config.toml:/etc/media-organizer/config.toml:ro \
     ghcr.io/jordanatdown/media-organizer:vX.Y.Z validate-config
   ```

2. **Essayer sur un input et une librairie jetables**, en `--dry-run` d'abord : il exécute
   l'inventaire, la planification et la résolution complets et affiche, pour chaque
   fichier, la **source de sa date** et sa destination. On voit exactement combien de
   fichiers seraient datés par le `mtime` (approximation) et où ils iraient.

   ```bash
   docker run --rm --user 1000:1000 -e TZ=Europe/Paris \
     -v ./config.toml:/etc/media-organizer/config.toml:ro \
     -v ./state:/var/lib/media-organizer \
     -v /mnt/nas/test/synchronisation:/inputs/test \
     -v /mnt/nas/test/phototeque:/libraries/test \
     ghcr.io/jordanatdown/media-organizer:vX.Y.Z --dry-run --once
   ```

   (avec `input = "test"` et `library = "test"` dans `config.toml`.)

3. Seulement ensuite, lancer en démon sur les vrais dossiers.

## Changement de comportement par rapport à la v1.2.4 de `syno-media-organizer`

- Un fichier sans date exploitable est **désormais rangé** d'après son `mtime` au lieu
  de rester dans le dossier de dépôt (`use_mtime_fallback = false` rétablit l'ancien contrat).
  Il peut donc atterrir dans un mois inexact — d'où le `warn` et le compteur dédié.
- Un doublon exact n'est plus renommé `_1` : il reste dans `input` et est mémorisé.
  Option `dedup.delete_source_duplicates = true` (défaut `false`) : la source est supprimée
  si elle est vérifiée identique octet par octet à la copie déjà rangée. Utile quand un client
  de synchronisation (Nextcloud iOS) ré-uploade un fichier après son rangement.
- Le cache `no_date_cache.json` du NAS contient des chemins `/volume1/…` qui n'existent
  plus vus du Pi : **on repart d'un cache vide**. Le premier scan journalise une fois les
  fichiers sans date, puis se tait.

## Ce qui disparaît côté NAS une fois migré

Le paquet DSM, l'utilisateur technique et ses ACL Synology sur les dossiers Nextcloud : le
Pi accède au NFS avec son propre uid. (À traiter dans le dépôt d'infrastructure, pas ici.)

## Limite connue : pas d'image amd64

L'image est volontairement arm64 seulement. Elle **ne peut pas tourner sur une VM de
staging x86_64**. Si l'infrastructure impose « staging avant prod », il faudra soit valider
directement sur le Pi contre un couple entrée/sortie jetable (en `--dry-run` d'abord, ce
qui est sûr), soit réintroduire une image amd64 pour le seul staging. C'est un arbitrage
à trancher au moment de la tâche de déploiement.
