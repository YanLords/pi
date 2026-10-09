//! Confinement des chemins et garde d'intégrité de la configuration.
//!
//! Le shell n'est *pas* un bac à sable : il est protégé par la confirmation (§32), pas par ces règles.

use agon_core::Digest;
use std::path::{Component, Path, PathBuf};

/// Ce qu'on peut faire d'un chemin situé dans le projet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Free,
    /// Lisible, pas modifiable (`.git`, `.agon`).
    ReadOnly,
    /// Ni lu ni modifié : secrets probables (`.env`, clés privées).
    Forbidden,
}

/// Dossiers jamais parcourus par la recherche.
pub const SKIPPED_DIRS: &[&str] = &[
    ".git",
    ".agon",
    "target",
    "node_modules",
    ".venv",
    "__pycache__",
];

/// Dossiers entiers de secrets : tout ce qu'ils contiennent est interdit.
const SECRET_DIRS: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube", ".docker"];

/// Modèles de configuration fournis sans secret : lisibles.
const SAFE_ENV_TEMPLATES: &[&str] = &[".env.example", ".env.sample", ".env.template", ".env.dist"];

/// `name` est déjà en minuscules (la comparaison est insensible à la casse : macOS et Windows le sont).
fn secret_name(n: &str) -> bool {
    if SAFE_ENV_TEMPLATES.contains(&n) {
        return false;
    }
    n == ".env"
        || n.starts_with(".env.")
        || n == ".envrc"
        || n.ends_with(".pem")
        || n.ends_with(".key")
        || n.ends_with(".p12")
        || n.ends_with(".pfx")
        || n.ends_with(".jks")
        || n.ends_with(".keystore")
        || n.ends_with(".kdbx")
        || n.ends_with(".tfvars")
        || n.ends_with(".tfstate")
        || n.ends_with(".tfstate.backup")
        || n.starts_with("id_rsa")
        || n.starts_with("id_ed25519")
        || n.starts_with("id_ecdsa")
        || n.starts_with("secrets.")
        || matches!(
            n,
            ".netrc"
                | ".npmrc"
                | ".pypirc"
                | ".pgpass"
                | ".htpasswd"
                | ".dockercfg"
                | ".git-credentials"
                | "credentials"
        )
}

/// Classe un chemin *relatif à la racine*, sans tenir compte de la casse.
pub fn classify(rel: &Path) -> Access {
    let comps: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str().map(str::to_lowercase),
            _ => None,
        })
        .collect();
    // `.git` à n'importe quelle profondeur (dépôts imbriqués) : config git avec URLs à jetons, hooks…
    if comps.iter().any(|c| c == ".git")
        || comps
            .iter()
            .any(|c| secret_name(c) || SECRET_DIRS.contains(&c.as_str()))
    {
        return Access::Forbidden;
    }
    if comps.first().is_some_and(|c| c == ".agon") {
        return Access::ReadOnly;
    }
    Access::Free
}

/// Résout `input` (relatif à `root`, ou absolu) en un chemin **dans** `root`.
/// `root` doit déjà être canonique. Suit les liens symboliques sur la partie existante du chemin.
pub fn resolve(root: &Path, input: &str) -> Result<PathBuf, String> {
    if input.trim().is_empty() {
        return Err("empty path".into());
    }
    if input.contains('\0') {
        return Err("path contains a NUL byte".into());
    }
    let p = Path::new(input);
    let full = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };

    // Plus long préfixe existant, canonisé (résout `..` et les liens) ; le reste ne doit contenir ni `..` ni racine.
    let mut existing = full.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_owned());
                existing = parent.to_path_buf();
            }
            _ => return Err(format!("`{input}` cannot be resolved")),
        }
    }
    if full.components().any(|c| matches!(c, Component::ParentDir)) && !rest.is_empty() {
        return Err(format!(
            "`{input}` uses `..` through a path that does not exist"
        ));
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|e| format!("`{input}`: {e}"))?;
    for name in rest.iter().rev() {
        resolved.push(name);
    }
    if !resolved.starts_with(root) {
        return Err(format!("`{input}` is outside the project"));
    }
    Ok(resolved)
}

pub fn relative<'a>(root: &Path, path: &'a Path) -> &'a Path {
    path.strip_prefix(root).unwrap_or(path)
}

/// Fichiers qui définissent les permissions, la vérification et la mémoire des mutations : l'agent ne doit pas pouvoir les
/// modifier, même par le shell (la confirmation humaine ne suffit pas à le garantir).
///
/// Le registre de mutations en fait partie : y glisser une entrée « validée » ferait rejouer, sans Jev,
/// une mutation choisie par l'agent.
const POLICY_FILES: &[&str] = &[
    "config.toml",
    "checks.toml",
    "extractors.toml",
    "candidates.toml",
    "mutations/registry.toml",
];

/// Instantané du contenu des fichiers de politique de `.agon/`.
pub struct Guard {
    root: PathBuf,
    before: Vec<(String, Option<Digest>)>,
}

impl Guard {
    pub fn snapshot(root: &Path) -> Self {
        Guard {
            root: root.to_path_buf(),
            before: Self::digests(root),
        }
    }

    fn digests(root: &Path) -> Vec<(String, Option<Digest>)> {
        POLICY_FILES
            .iter()
            .map(|f| {
                (
                    f.to_string(),
                    std::fs::read(root.join(".agon").join(f))
                        .ok()
                        .map(|b| Digest::of_bytes(&b)),
                )
            })
            .collect()
    }

    /// Fichiers de politique modifiés, créés ou supprimés depuis l'instantané.
    pub fn violations(&self) -> Vec<String> {
        let now = Self::digests(&self.root);
        self.before
            .iter()
            .zip(now)
            .filter(|(a, b)| a.1 != b.1)
            .map(|(a, _)| format!(".agon/{}", a.0))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agon-sbx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn paths_inside_the_project_resolve_including_new_files() {
        let root = scratch("inside");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "x").unwrap();
        assert_eq!(resolve(&root, "src/a.rs").unwrap(), root.join("src/a.rs"));
        assert_eq!(
            resolve(&root, "src/new/deep/b.rs").unwrap(),
            root.join("src/new/deep/b.rs")
        );
        assert_eq!(
            resolve(&root, "src/../src/a.rs").unwrap(),
            root.join("src/a.rs")
        );
        assert_eq!(
            resolve(&root, root.join("src/a.rs").to_str().unwrap()).unwrap(),
            root.join("src/a.rs")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn escapes_are_refused() {
        let root = scratch("escape");
        for bad in [
            "../outside",
            "src/../../outside",
            "/etc/passwd",
            "/",
            "",
            "a\0b",
            "new/../../x",
        ] {
            assert!(resolve(&root, bad).is_err(), "{bad:?} must be refused");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlink_pointing_outside_is_refused_even_for_new_files_below_it() {
        let root = scratch("symlink");
        let outside = scratch("symlink-outside");
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert!(resolve(&root, "link").is_err());
        assert!(resolve(&root, "link/secret.txt").is_err());
        assert!(resolve(&root, "link/new/file.txt").is_err());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn a_symlink_inside_the_project_is_fine() {
        let root = scratch("symlink-ok");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
        assert_eq!(
            resolve(&root, "alias/x.txt").unwrap(),
            root.join("real/x.txt")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn secrets_and_agon_state_are_classified() {
        for secret in [
            ".env",
            ".env.local",
            "config/.env",
            "server.pem",
            "id_rsa",
            "deploy.key",
            ".git/config",
            ".netrc",
        ] {
            assert_eq!(classify(Path::new(secret)), Access::Forbidden, "{secret}");
        }
        assert_eq!(classify(Path::new(".agon/config.toml")), Access::ReadOnly);
        assert_eq!(
            classify(Path::new(".agon/sessions/s1.jsonl")),
            Access::ReadOnly
        );
        for ok in [
            "src/main.rs",
            "README.md",
            "environment.md",
            "keys.rs",
            "a/.gitignore",
        ] {
            assert_eq!(classify(Path::new(ok)), Access::Free, "{ok}");
        }
    }

    #[test]
    fn the_guard_detects_changed_created_and_deleted_policy_files() {
        let root = scratch("guard");
        std::fs::create_dir_all(root.join(".agon")).unwrap();
        std::fs::write(root.join(".agon/config.toml"), "a = 1").unwrap();
        std::fs::write(root.join(".agon/checks.toml"), "b = 2").unwrap();
        let g = Guard::snapshot(&root);
        assert!(g.violations().is_empty());

        std::fs::write(root.join(".agon/config.toml"), "a = 2").unwrap(); // modifié
        std::fs::remove_file(root.join(".agon/checks.toml")).unwrap(); // supprimé
        std::fs::write(root.join(".agon/extractors.toml"), "c = 3").unwrap(); // créé
        std::fs::write(root.join(".agon/registry.toml"), "harmless").unwrap(); // hors politique
        assert_eq!(
            g.violations(),
            [
                ".agon/config.toml",
                ".agon/checks.toml",
                ".agon/extractors.toml"
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
