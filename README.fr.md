# Quick

*[English version](./README.md)*

Quick transforme n'importe quel dossier, ZIP ou ensemble de fichiers en site web
en ligne en quelques secondes : déposez-les, choisissez un nom, et votre site est
instantanément accessible à sa propre adresse — sans étape de compilation, sans
configuration, sans serveur à gérer.

À l'ère de l'IA générative, nous produisons toujours plus de pages, de
prototypes et de petits sites. Quick répond à ce besoin : héberger ces
productions de plus en plus nombreuses, toujours plus facilement et plus vite.

C'est une implémentation Rust/Pingora de l'idée centrale de
[Shopify Quick](https://shopify.engineering/quick) : déposer des fichiers et
obtenir immédiatement un site accessible via son propre sous-domaine, avec un
seul binaire.

![screenshot](./assets/screenshot.png)

## Fonctionnalités

- remplacement transactionnel de site statique avec retour arrière en cas
  d'échec ;
- interface de déploiement avec sélection de fichiers et glisser-déposer ;
- interface en français ou en anglais selon l'en-tête `Accept-Language`, avec
  l'anglais comme langue de secours et un sélecteur manuel persistant ;
- upload et extraction ZIP sécurisés ;
- interface entièrement embarquée dans l'exécutable, sans fichier externe
  requis ;
- configuration du nom d'hôte avec accès immédiat à l'URL publiée ;
- routage Pingora via `<site>.<domaine>` ;
- support des `index.html` de répertoire ;
- fallback `index.html` pour les applications monopages ;
- listing automatique des fichiers en l'absence d'`index.html` à la racine ;
- rendu React et Babel côté navigateur pour les composants `.jsx` ;
- détection automatique des types MIME ;
- rejet des liens symboliques et des tentatives de path traversal ;
- stockage local ou compatible S3 avec publications atomiques.

Quick n'implémente pas encore les API base de données, fichier, IA, entrepôt de
données, WebSocket ou identité décrites par Shopify.

## Démarrage rapide

```sh
cargo build
cargo run -- serve
```

Ouvrez <http://localhost:8080>, sélectionnez des fichiers, un dossier ou une
archive ZIP, puis choisissez un nom d'hôte. Le site sera disponible à
`http://<nom>.localhost:8080`.

Quand tous les fichiers d'une archive ZIP se trouvent sous un seul répertoire
racine, Quick supprime automatiquement ce répertoire. Les archives sont limitées
à 1 000 entrées et 25 Mio de contenu décompressé. Les chemins d'échappement,
les liens symboliques et les fichiers chiffrés sont rejetés.

Tous les types de fichiers sont acceptés. Quand la racine ne contient pas
d'`index.html`, Quick affiche un listing avec un lien vers chaque fichier
déposé. L'ouverture d'un fichier `.jsx` affiche l'export React par défaut. Un
fichier contenant uniquement une expression JSX est également supporté. Le rendu
JSX charge React et Babel depuis des CDN publics : le navigateur doit donc avoir
accès à Internet.

Grâce au support natif de JSX, les artefacts Claude — composants React
interactifs générés directement par Claude — peuvent être enregistrés en `.jsx`
et mis en ligne sans aucune étape de compilation. Il suffit de copier le code de
l'artefact, de le déposer, et il est immédiatement accessible via son propre
sous-domaine.

Les déploiements en ligne de commande restent disponibles :

```sh
cargo run -- deploy examples/hello --site hello
```

Puis ouvrez <http://hello.localhost:8080>. Les navigateurs modernes résolvent
généralement `*.localhost` vers `127.0.0.1`. Sinon :

```sh
curl -H 'Host: hello.localhost' http://127.0.0.1:8080/
```

Options utiles :

```sh
quick serve --listen 0.0.0.0:8080 --sites-dir ./sites --base-domain quick.internal
quick deploy ./dist --site my-site --sites-dir ./sites
```

## Sécurité

Quick sert du HTTP simple, sans authentification ni TLS. Le serveur entier doit
être placé derrière un proxy d'authentification tel qu'un
[Identity-Aware Proxy (IAP) Google Cloud](https://cloud.google.com/iap) ou
[oauth2-proxy](https://github.com/oauth2-proxy/oauth2-proxy) dès qu'il est
exposé en dehors d'un réseau de confiance. Toute personne pouvant accéder à
l'interface peut créer ou remplacer un site.

## Stockage S3

Quick peut stocker les sites dans AWS S3 ou un service compatible tel que
MinIO, Cloudflare R2 ou Backblaze B2. Chaque déploiement est écrit sous un
préfixe de release immuable et publié en remplaçant `current.json`.

Les identifiants utilisent la chaîne de credentials AWS standard :
`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, profils et
rôles d'instance.

AWS S3 :

```sh
quick serve \
  --storage s3 \
  --s3-bucket quick-sites \
  --s3-region eu-west-1
```

MinIO ou un autre endpoint compatible :

```sh
AWS_ACCESS_KEY_ID=minio \
AWS_SECRET_ACCESS_KEY=minio-secret \
quick serve \
  --storage s3 \
  --s3-bucket quick-sites \
  --s3-region us-east-1 \
  --s3-endpoint http://127.0.0.1:9000 \
  --s3-path-style
```

Les options sont également disponibles via `QUICK_STORAGE`, `QUICK_S3_BUCKET`,
`QUICK_S3_REGION`, `QUICK_S3_ENDPOINT`, `QUICK_S3_PREFIX` et
`QUICK_S3_PATH_STYLE`. Le bucket doit exister avant le démarrage de Quick.

Politique IAM minimale pour le préfixe par défaut :

```json
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Action": ["s3:GetObject", "s3:PutObject"],
    "Resource": "arn:aws:s3:::quick-sites/quick/*"
  }]
}
```

Les releases précédentes restent immuables dans le bucket. Configurez une règle
de cycle de vie S3 sur `quick/sites/*/releases/` pour les supprimer après la
période de rétention souhaitée.

## Architecture

```text
quick deploy ./dist --site demo
             |
             v
 local: sites/demo/*
   ou
 S3: quick/sites/demo/releases/<id>/*
             |
             v
demo.localhost -> Pingora -> fichier statique
```

## Tests

```sh
cargo test
```

## Releases

Les tags sémantiques correspondant à `vX.Y.Z` déclenchent le workflow de
release GitHub Actions. La version du tag doit correspondre à celle de
`Cargo.toml`.

```sh
git tag v0.1.0
git push origin v0.1.0
```

Après les vérifications de formatage, Clippy et les tests, GitHub publie une
release avec :

- Linux x86_64 en archive `.tar.gz` ;
- macOS Intel en archive `.tar.gz` ;
- macOS Apple Silicon en archive `.tar.gz` ;
- Windows x86_64 avec `quick.exe` dans une archive `.zip` ;
- un checksum SHA-256 pour chaque archive.

Chaque archive contient l'exécutable et ce README. Le workflow démarre chaque
binaire sur son runner GitHub natif et vérifie l'interface embarquée avant
de publier la release.

Le workflow peut également être lancé manuellement depuis GitHub Actions. Dans
ce cas, il compile et stocke les artefacts sans créer de GitHub Release.

Chaque build vérifie que l'interface est bien embarquée dans l'exécutable final.
Le job Linux exécute également une copie isolée de l'exécutable depuis un
répertoire vide et vérifie que la page d'administration fonctionne sans fichiers
compagnons.
