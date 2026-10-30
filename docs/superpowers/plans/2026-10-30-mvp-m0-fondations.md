# MVP — M0 Fondations : plan d'implémentation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Poser le dépôt du produit : workspace Rust, règles de qualité, outil de traçabilité exigences ↔ tests avec garde CI, et **référentiel d'exigences du MVP** soumis à validation.

**Architecture:** Workspace Cargo à la racine (`crates/`, `xtask/`). La crate `ma-trace` fournit l'attribut `#[covers("ID")]` (sans effet à l'exécution). `cargo xtask trace` lit `requirements/**/*.yaml`, scanne les tests Rust et TS, croise avec les résultats d'exécution, produit la matrice et applique la garde. `cargo xtask ci` enchaîne localement toutes les vérifications ; un workflow GitHub Actions l'appelle sur Ubuntu 24.04 et Fedora.

**Tech Stack:** Rust stable (édition 2024), `syn` / `quote` / `proc-macro2`, `serde` + une crate YAML maintenue (`serde_yaml_ng` ou `serde_norway`), `quick-xml` (JUnit), `serde_json`, `clap`, `walkdir`, `cargo-nextest`, `cargo-llvm-cov`, `cargo-deny`, mold.

**Spec:** `docs/02-stack-technique.md` (§2.1, §5, §6), `docs/05-feuille-de-route-mvp.md`, `docs/01-analyse-fonctionnelle.md` (§0, §4, §6), `docs/03-brief-design.md` + `docs/design/`, `docs/04-resultats-spike.md` (§1, §4).

## Global Constraints

- Licence **GPL-3.0-or-later** (fichier `LICENSE`, champ `license` de chaque crate).
- Rust stable, **édition 2024** ; `#![forbid(unsafe_code)]` dans toutes les crates sauf les crates de liaison (aucune en M0).
- `cargo clippy --all-targets -- -D warnings` et `cargo fmt --check` doivent passer.
- `cargo deny check` : licences compatibles GPL-3 uniquement, avis de sécurité bloquants.
- Couverture minimale de **90 % des lignes** pour les crates du moteur (`crates/ma-*`, hors `ma-trace` qui est une macro sans logique). En M0, `xtask` n'est pas soumis au seuil.
- Construction documentée et vérifiée sur **Ubuntu 24.04** et **Fedora**.
- Identifiants d'exigence **stables** : regex `^[A-Z][A-Z0-9]*(\.[A-Z0-9]+)*\.[0-9]{2}$` (ex. `TS.TR1.CC.01`).
- Langue des documents et des messages : français. Identifiants et code : anglais.
- Le code jetable du spike (branche `spike/stack-validation`) **n'est pas réutilisé tel quel** ; on n'en reprend que les principes (cf. `04-resultats-spike.md` §4).

## Review Focus

1. **Exigence sans aucun test lié, ou test qui référence un identifiant inconnu** : la garde doit échouer avec un message qui nomme l'identifiant et le fichier (tests des Tâches 4 et 5).
2. **Fichier YAML malformé ou identifiant en double** : erreur claire avec fichier et ligne, pas de panique (Tâche 3).
3. **Un test lié existe mais échoue** : l'exigence ne doit pas compter comme couverte (Tâche 5).
4. **Attribut `#[covers]` sur autre chose qu'une fonction** (module, `impl`), ou appel `covers(...)` construit dynamiquement en TS : le scanner les ignore avec un avertissement, sans planter (Tâche 4).
5. **Résultats d'exécution absents** (tests non lancés) : la matrice indique « non exécuté », et la garde de livraison échoue (Tâche 5).

---

## Structure des fichiers

```
Cargo.toml                    # workspace : crates/*, xtask
rust-toolchain.toml           # channel = "stable", components rustfmt, clippy, llvm-tools-preview
.cargo/config.toml            # alias xtask ; linker mold (x86_64-unknown-linux-gnu)
deny.toml                     # licences autorisées, avis
LICENSE                       # GPL-3.0
BUILD.md                      # dépendances apt / dnf, commandes
crates/ma-trace/              # proc-macro #[covers("ID", ...)]
xtask/src/
  main.rs                     # sous-commandes : trace, ci
  requirements.rs             # modèle + chargement + validation YAML
  scan_rust.rs                # liens Rust (#[covers])
  scan_ts.rs                  # liens TS (covers("ID"))
  results.rs                  # JUnit (nextest) + JSON (Playwright/Vitest)
  matrix.rs                   # matrice Markdown/HTML + garde
  ci.rs                       # pipeline local
xtask/tests/fixtures/         # jeux d'essai du scanner et de la garde
requirements/
  config.yaml                 # current_lot: MVP
  mvp/*.yaml                  # référentiel MVP (Tâche 6)
.github/workflows/ci.yml
```

---

### Task 1 : Workspace, outillage et règles de qualité

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `.cargo/config.toml`, `deny.toml`, `LICENSE`, `BUILD.md`, `xtask/Cargo.toml`, `xtask/src/main.rs`
- Modify: `.gitignore` (ajouter `/target`, `node_modules/`)

**Interfaces:**
- Produces : alias `cargo xtask <cmd>` (`.cargo/config.toml` : `[alias] xtask = "run -p xtask --"`) ; workspace `members = ["crates/*", "xtask"]`, `resolver = "3"` ; `[workspace.package] edition = "2024"`, `license = "GPL-3.0-or-later"` ; `[workspace.lints.rust] unsafe_code = "forbid"` ; `[workspace.lints.clippy]` groupe `pedantic` en `warn`, avec `module_name_repetitions` et `missing_errors_doc` autorisées.

- [ ] **Step 1 : Écrire les fichiers.**
  - `xtask/src/main.rs` : CLI `clap` avec les sous-commandes `trace` et `ci` (corps `todo!()` remplacés aux Tâches 5 et 7) et `--help`.
  - `BUILD.md` : les paquets apt de `spike/README.md` (sans TSDuck ni outils de test, regroupés à part), leur équivalent **dnf** pour Fedora, l'installation de `cargo-nextest`, `cargo-llvm-cov`, `cargo-deny` et la commande `cargo xtask ci`.
  - `deny.toml` : `allow` = MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, MPL-2.0, LGPL-2.1-or-later, LGPL-3.0, GPL-3.0-or-later ; avis bloquants.
- [ ] **Step 2 : Vérifier.**
  Run: `cargo build --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check && cargo deny check && cargo xtask --help`
  Expected : tout réussit ; l'aide liste `trace` et `ci`.
- [ ] **Step 3 : Vérifier la construction sous Fedora.**
  Run: `podman run --rm -v $PWD:/src:Z -w /src fedora:latest bash -c "<commandes dnf de BUILD.md> && curl https://sh.rustup.rs -sSf | sh -s -- -y && . ~/.cargo/env && cargo build --workspace"` (ou `docker`).
  Expected : build réussi. Si ni podman ni docker n'est installé, le noter dans le ledger, et la vérification Fedora passe au job CI de la Tâche 7.
- [ ] **Step 4 : Commit** : `git add -A && git commit -m "build: workspace, règles de qualité, licence GPL-3"`

---

### Task 2 : Crate `ma-trace` (attribut `#[covers]`)

**Files:**
- Create: `crates/ma-trace/Cargo.toml` (`proc-macro = true`), `crates/ma-trace/src/lib.rs`
- Test: `crates/ma-trace/tests/covers.rs`

**Interfaces:**
- Produces : `#[ma_trace::covers("ID1", "ID2", …)]`, applicable à une `fn`. Il renvoie l'élément inchangé. Il refuse à la compilation (`compile_error!`) une liste vide ou un littéral qui ne respecte pas la regex des identifiants.

- [ ] **Step 1 : Test qui échoue** — `covers.rs` :
  - `#[covers("TS.TR1.CC.01")] #[test] fn annotated_test_still_runs() { assert_eq!(2 + 2, 4) }` ;
  - `#[covers("A.01", "B.C.02")] fn helper() -> u8 { 7 }` ;
  - `#[test] fn attribute_keeps_function_unchanged() { assert_eq!(helper(), 7) }`.
- [ ] **Step 2 : Vérifier l'échec** — Run: `cargo test -p ma-trace` ; Expected : FAIL (attribut inconnu).
- [ ] **Step 3 : Implémenter** `pub fn covers(attr: TokenStream, item: TokenStream) -> TokenStream` : analyse de l'attribut avec `syn` (`Punctuated<LitStr, Comma>`), validation de la regex (implémentée à la main, sans dépendance `regex`), puis retour de `item`.
- [ ] **Step 4 : Cas d'erreur** — ajouter `tests/ui/bad_id.rs` et `tests/ui/empty.rs`, vérifiés avec `trybuild` (`t.compile_fail("tests/ui/*.rs")`) ; les fichiers `.stderr` attendus contiennent `identifiant d'exigence invalide` et `au moins un identifiant`.
- [ ] **Step 5 : Tout passe** — Run: `cargo test -p ma-trace` ; Expected : PASS.
- [ ] **Step 6 : Commit.**

---

### Task 3 : Référentiel d'exigences : modèle, chargement, validation

**Files:**
- Create: `xtask/src/requirements.rs`, `requirements/config.yaml`
- Test: `xtask/src/requirements.rs` (module `tests`), fixtures `xtask/tests/fixtures/req/{ok,dup,bad_id,bad_yaml}/*.yaml`

**Interfaces:**
- Produces :
  - `pub enum Lot { Mvp, V1, V2, V3, V4, V5, Later }` (YAML : `MVP`, `V1`… `LATER`) ;
  - `pub enum Level { Unit, Ui, E2e }` (YAML : `unit`, `ui`, `e2e`) ;
  - `pub enum Status { Draft, Validated, Implemented }` ;
  - `pub struct Requirement { pub id: String, pub title: String, pub description: String, pub domain: String /* F1..F18 | TRANS */, pub lot: Lot, pub acceptance: Vec<String>, pub levels: Vec<Level>, pub source: String, pub status: Status, pub file: PathBuf, pub line: usize }` ;
  - `pub struct Config { pub current_lot: Lot }` ;
  - `pub fn load(dir: &Path) -> Result<(Config, Vec<Requirement>), Vec<Diagnostic>>` ;
  - `pub struct Diagnostic { pub file: PathBuf, pub line: Option<usize>, pub message: String }`.

Format d'un fichier : une liste YAML d'objets avec les champs ci-dessus (sauf `file` et `line`, ajoutés au chargement ; `line` est la ligne de l'élément `- id:`).

- [ ] **Step 1 : Tests qui échouent** :
  - `loads_valid_requirements` : le dossier `ok/` (2 fichiers, 3 exigences) donne 3 éléments, dont les champs et la ligne de la 2e exigence sont vérifiés ;
  - `rejects_duplicate_id` : `dup/` donne un diagnostic dont le message contient `identifiant en double` et les deux fichiers ;
  - `rejects_invalid_id` : `bad_id/` (`ts.tr1.01`) donne un diagnostic avec le fichier et la ligne ;
  - `reports_malformed_yaml_without_panic` : `bad_yaml/` donne un diagnostic avec le fichier ;
  - `rejects_empty_acceptance_or_levels` : une exigence sans critère d'acceptation ou sans niveau de test est refusée.
- [ ] **Step 2 : Vérifier l'échec** — Run: `cargo test -p xtask requirements` ; Expected : FAIL.
- [ ] **Step 3 : Implémenter** `load` (parcours `walkdir`, désérialisation `serde`, lignes obtenues en repérant `- id:` dans le texte source).
- [ ] **Step 4 : Tout passe** — même commande ; Expected : PASS.
- [ ] **Step 5 : Commit.**

---

### Task 4 : Scanners de liens tests → exigences

**Files:**
- Create: `xtask/src/scan_rust.rs`, `xtask/src/scan_ts.rs`
- Test: modules `tests` des deux fichiers, fixtures `xtask/tests/fixtures/scan/{rust,ts}/`

**Interfaces:**
- Produces :
  - `pub struct TestLink { pub req_id: String, pub test_name: String /* Rust : chemin module::fn ; TS : titre du test */, pub level: Level, pub file: PathBuf, pub line: usize }` ;
  - `pub fn scan_rust(root: &Path) -> (Vec<TestLink>, Vec<Diagnostic>)` : parcourt `crates/**/src/**/*.rs` et `crates/**/tests/**/*.rs`. Un `#[covers(...)]` posé sur une `fn` portant `#[test]` (ou `#[tokio::test]`, `#[rstest]`) donne un lien de niveau `Unit` ;
  - `pub fn scan_ts(root: &Path) -> (Vec<TestLink>, Vec<Diagnostic>)` : appels `covers("ID", …)` à l'intérieur d'un `test(...)` / `it(...)`. Les fichiers `ui/src/**/*.test.ts(x)` donnent le niveau `Ui`, `ui/tests/**/*.spec.ts` (Playwright) le niveau `Ui`, `e2e/**/*.e2e.ts` le niveau `E2e`. Le nom du test est le premier argument littéral de `test` / `it`.

- [ ] **Step 1 : Tests qui échouent** :
  - `rust_links_annotated_tests` : la fixture contient 2 tests annotés (dont un avec 2 identifiants) et 1 non annoté ⇒ 3 liens, avec chemin et ligne corrects ;
  - `rust_ignores_covers_on_non_fn_with_warning` : `#[covers]` sur un `mod` donne un diagnostic d'avertissement et aucun lien ;
  - `rust_ignores_covers_without_test_attribute_with_warning` ;
  - `ts_links_covers_calls_by_level` : 3 fichiers fixtures (`*.test.tsx`, `*.spec.ts`, `*.e2e.ts`) ⇒ niveaux Ui, Ui, E2e ;
  - `ts_warns_on_non_literal_ids` : `covers(id)` donne un avertissement.
- [ ] **Step 2 : Vérifier l'échec** — Run: `cargo test -p xtask scan` ; Expected : FAIL.
- [ ] **Step 3 : Implémenter.**
  - Rust : `syn::parse_file` puis visite des `ItemFn`, y compris dans les `mod` en ligne.
  - TS : analyse lexicale légère. On repère `test(` / `it(` et leur titre littéral, puis les `covers(` avec des chaînes littérales jusqu'à la fermeture du bloc (comptage des parenthèses et accolades, en ignorant les chaînes et les commentaires). Pas de dépendance à un parseur JS.
- [ ] **Step 4 : Tout passe** ; Expected : PASS.
- [ ] **Step 5 : Commit.**

---

### Task 5 : Résultats d'exécution, matrice et garde

**Files:**
- Create: `xtask/src/results.rs`, `xtask/src/matrix.rs`, `.config/nextest.toml` (profil `ci`), `requirements/tooling.yaml` ; Modify: `xtask/src/main.rs` (sous-commande `trace`), `crates/ma-trace/tests/covers.rs`
- Test: modules `tests`, fixtures `xtask/tests/fixtures/results/` (JUnit nextest, JSON Playwright, JSON Vitest)

**Interfaces:**
- Consumes : `load`, `Requirement`, `Config` (Tâche 3) ; `TestLink`, `scan_rust`, `scan_ts` (Tâche 4).
- Produces :
  - `pub enum Outcome { Passed, Failed, Skipped, NotRun }` ;
  - `pub fn load_results(paths: &[PathBuf]) -> HashMap<String /* test_name */, Outcome>` : JUnit `testcase@classname::name`, Playwright `suites[].specs[].title` + `tests[].results[].status`, Vitest JSON `testResults[].assertionResults[].fullName/status` ;
  - `pub struct Coverage { pub req: Requirement, pub links: Vec<(TestLink, Outcome)> }` ;
  - `pub fn evaluate(reqs, links, results, current: Lot, release: bool) -> (Vec<Coverage>, Vec<Diagnostic> /* erreurs de garde */)` ;
  - `pub fn render_markdown(cov: &[Coverage]) -> String` et `pub fn render_html(cov: &[Coverage]) -> String` ;
  - CLI : `cargo xtask trace [--results <fichier>...] [--release] [--out target/trace]`. Écrit `matrix.md` et `matrix.html`, renvoie le code 1 si la garde échoue.

**Règles de la garde** (lot courant = `config.yaml`) :
1. Un lien vers un identifiant inconnu est une **erreur**.
2. Une exigence du lot courant en statut `implemented` doit avoir **au moins un test qui passe à chacun des niveaux** de `levels`. Sinon, c'est une erreur qui nomme l'exigence et les niveaux manquants.
3. Avec `--release`, **toutes** les exigences du lot courant doivent être `implemented` et satisfaire la règle 2.
4. Les exigences `draft` / `validated` apparaissent dans la matrice comme « à faire », sans erreur hors `--release`.
5. Un test lié mais absent des résultats compte comme `NotRun`, donc non couvrant.

- [ ] **Step 1 : Tests qui échouent** :
  - `unknown_id_fails_gate` ;
  - `implemented_requirement_needs_passing_test_per_level` (niveaux Unit + Ui, seul le test Unit passe ⇒ erreur qui mentionne `ui`) ;
  - `failed_linked_test_does_not_cover` ;
  - `not_run_test_does_not_cover` ;
  - `release_requires_all_current_lot_implemented` ;
  - `other_lots_are_ignored_by_gate` ;
  - `parses_nextest_junit`, `parses_playwright_json`, `parses_vitest_json` ;
  - `markdown_matrix_lists_each_requirement_with_status` (la sortie contient l'identifiant, le titre, les niveaux, les noms des tests et ✓ / ✗ / ∅).
- [ ] **Step 2 : Vérifier l'échec** — Run: `cargo test -p xtask` ; Expected : FAIL.
- [ ] **Step 3 : Implémenter** `results.rs`, `matrix.rs` et la sous-commande.
- [ ] **Step 4 : Tout passe** — Run: `cargo test -p xtask && cargo xtask trace` ; Expected : PASS ; `target/trace/matrix.md` est généré (référentiel encore vide ⇒ matrice vide, code 0).
- [ ] **Step 5 : Annoter les tests de `ma-trace`** avec des identifiants d'outillage (`TOOL.TRACE.01` : attribut sans effet ; `TOOL.TRACE.02` : refus des identifiants invalides), et créer `requirements/tooling.yaml` (lot `MVP`, statut `implemented`, niveau `unit`). Ce sont les premières exigences réellement tracées.
  Run: `cargo nextest run --workspace --profile ci && cargo xtask trace --results target/nextest/ci/junit.xml` ; Expected : code 0, avec `TOOL.TRACE.01/02` ✓ dans la matrice.
  (Profil `ci` de nextest avec `junit.path = "junit.xml"`, dans `.config/nextest.toml`.)
- [ ] **Step 6 : Commit.**

---

### Task 6 : Référentiel d'exigences du MVP (soumis à validation)

**Files:**
- Create: `requirements/mvp/{structure,navigation,image,donnees,syntaxe,transport,rapport,reglages,i18n,performance,interface}.yaml`

**Règles de rédaction :**
- **Schéma d'identifiants** : préfixe de domaine + sous-domaine + numéro.
  `STRUCT` (F1), `NAV` (F2), `IMG` (F3), `DATA` (F4), `SYN` (F5), `TS` (F6 ; sous-domaines `TR1`, `TR2`, `BR`), `RPT` (F17), `SET` (F18), `I18N`, `PERF`, `UI` (transverse : code couleur, infobulles, synchronisation, clavier, états).
- **Une exigence = un comportement vérifiable.** Chaque critère d'acceptation est formulé pour pouvoir être testé (entrée → résultat attendu, valeurs chiffrées quand la spec en donne).
- `source` cite la section précise : `01 §F6`, `02 §3.1`, `03 §4.6`, `04 §1`, maquette `design/maquettes/TransportView.dc.html`, etc.
- `levels` : `unit` pour la logique du moteur ; `ui` pour un comportement d'interface testable sur mock ; `e2e` pour ce qui exige l'application réelle et le corpus. Au moins un niveau.
- Toutes les exigences sont créées en statut **`draft`**. Elles passeront en `validated` après la revue du porteur du projet.

**Contenu minimal obligatoire** (une exigence au moins par point) :
- **STRUCT** : ouverture TS 188/204/208, TTS et ES bruts H.264/HEVC/MPEG-2 ; détection du format ; arbre PAT/CAT/SDT/PMT/ES avec PID, infos et porteur du PCR ; nom de service via la SDT ; flux non supporté libellé « Vidéo » / « Audio » ; MPTS ; sélection automatique de la première vidéo.
- **NAV** : vignettes avec type (couleurs I/P/B), index de décodage et de présentation, paramètres présents ; boutons début / I précédente / précédente / suivante / I suivante / fin ; Aller à ; modes RAP et I ; barre de position avec repères d'erreurs et partie non indexée hachurée ; raccourcis clavier (`03 §2`, `HANDOFF.md` §4) ; **image affichée = image demandée** (oracle par code incrusté), y compris B-frames, GOP ouvert et point de reprise H.264.
- **IMG** : affichage de l'image décodée ; zoom 50–400 % sans lissage ; Y/U/V ; grilles de slices et tiles ; overlays futurs visibles mais désactivés ; état « non décodable » avec action « aller à l'image I suivante ».
- **DATA** : champs Séquence/Image et Transport de `01 §F4` limités au MVP (sans statistiques de prédiction) ; valeurs copiables.
- **SYN** : deux arbres (paramètres actifs / AU courante) ; colonnes élément, valeur, longueur, offset, interprétation ; décimal / hexadécimal ; recherche ; vue hexadécimale synchronisée ; éléments en erreur en rouge avec la règle citée.
- **TS** : **chaque indicateur** TR 101 290 P1 (6) et P2 (6), avec la condition de déclenchement de `01 §F6` et les seuils de `01 §F6` / `02` ; activation et seuils réglables ; prise en compte ou non de `transport_error_indicator` ; débits min / moy / max par PID et graphe ; **oracle TSDuck** sur le corpus avec défauts injectés.
- **RPT** : colonnes, filtres, recherche, compteurs, regroupement des répétitions, mise en sourdine après N, navigation par double-clic, export CSV (format machine stable) et texte, stockage paginé (millions de messages).
- **SET** : sections du MVP (Affichage, Journaux, TS / TR 101 290, Vidéo dont le décodage matériel), « tout cocher », persistance TOML.
- **I18N** : FR par défaut (ou langue du système), EN complet, bascule à chaud, aucune clé manquante, pseudo-localisation, éléments de syntaxe non traduits, formats de nombres selon la langue et exports stables.
- **PERF** (seuils de `02 §8`, conditions de `04 §1`) : première image ≤ 2 s avec indexation en fond ; saut ≤ 500 ms p95 ; image suivante et précédente **côté interface** ≤ 100 ms p95, **sans arrêt** en navigation arrière ; mémoire ≤ 2 Go ; index persistant (réouverture instantanée) ; fichiers de plusieurs Go locaux et sur partage réseau ; jobs annulables avec progression ; interface jamais figée ; repli logiciel détecté et signalé.
- **UI** : disposition et panneaux masquables ou redimensionnables (`03 §3`) ; code couleur avec forme et couleur (daltonisme) ; infobulles sur chaque élément ; synchronisation de toutes les vues sur l'image courante ; écran d'accueil et fichiers récents ; états transverses (`03 §4.11`) ; tenue en 1366×768 ; thème sombre par défaut et thème clair.

- [ ] **Step 1 : Rédiger les fichiers** selon les règles ci-dessus.
- [ ] **Step 2 : Valider la forme** — Run: `cargo xtask trace` ; Expected : code 0 ; la matrice liste toutes les exigences MVP en « à faire », et aucun diagnostic de chargement n'apparaît.
- [ ] **Step 3 : Contrôle de complétude** — pour chaque puce du « Contenu minimal obligatoire », retrouver au moins un identifiant dans `matrix.md`, et consigner la correspondance (puce → identifiants) dans le ledger.
- [ ] **Step 4 : Commit** : `git commit -m "exigences: référentiel MVP (draft)"`.
- [ ] **Step 5 : Point de passage** — présenter au porteur la matrice (`target/trace/matrix.html`, publiable en artefact) et un résumé par domaine (nombre d'exigences, points délicats). Sur sa validation, passer les statuts à `validated` et commiter. **C'est une étape bloquante pour la suite du MVP** (M1 et suivants).

---

### Task 7 : CI locale et workflow

**Files:**
- Create: `xtask/src/ci.rs`, `.github/workflows/ci.yml` ; Modify: `xtask/src/main.rs`

**Interfaces:**
- Consumes : la sous-commande `trace` (Tâche 5).
- Produces : `cargo xtask ci [--release]`, qui enchaîne dans cet ordre et s'arrête au premier échec, en affichant l'étape :
  1. `cargo fmt --check` ;
  2. `cargo clippy --workspace --all-targets -- -D warnings` ;
  3. `cargo deny check` ;
  4. `cargo llvm-cov nextest --workspace --profile ci --lcov --output-path target/lcov.info`, puis contrôle du seuil de **90 %** par crate `crates/ma-*` sauf `ma-trace` (lecture du lcov ; liste des crates sous le seuil) ;
  5. `cargo xtask trace --results target/nextest/ci/junit.xml [--release]`.

  Les étapes UI (Vitest, Playwright) et bout en bout s'ajouteront en M6 / M7 via le même mécanisme : une liste d'étapes dans `ci.rs`.

- [ ] **Step 1 : Test qui échoue** — `coverage_gate_lists_crates_below_threshold` : un lcov fixture avec deux crates (95 % et 80 %) ⇒ échec qui nomme la seconde.
- [ ] **Step 2 : Vérifier l'échec**, puis implémenter `pub fn coverage_by_crate(lcov: &str, root: &Path) -> BTreeMap<String, f64>` et le pipeline.
- [ ] **Step 3 : Tout passe en local** — Run: `cargo xtask ci` ; Expected : toutes les étapes vertes (le seuil de couverture ne s'applique encore à aucune crate moteur).
- [ ] **Step 4 : Workflow** `.github/workflows/ci.yml` : deux jobs.
  - `ubuntu-24.04` : paquets de `BUILD.md`, installation des outils cargo, `cargo xtask ci`.
  - `fedora` : `container: fedora:latest`, paquets dnf de `BUILD.md`, `cargo build --workspace` puis `cargo nextest run --workspace`.

  Cache `~/.cargo` et `target`. Le workflow n'est pas exécuté tant que le dépôt n'a pas de remote GitHub ; le noter dans le ledger.
- [ ] **Step 5 : Commit.**
