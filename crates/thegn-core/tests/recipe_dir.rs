//! WS00-P1-08 guard: recipes live only under `config/recipes/`, which the nix
//! source filter already includes through its `config` root. A top-level
//! `recipes/` directory would be a second home for config data and would be
//! missing from the sandboxed build (resolutions B4).

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The entries of the `roots = [ … ];` list in `nix/source.nix`, unquoted.
fn nix_source_roots() -> Vec<String> {
    let body =
        std::fs::read_to_string(repo_root().join("nix/source.nix")).expect("read nix/source.nix");
    let start = body
        .find("roots = [")
        .expect("nix/source.nix has a roots list");
    let list = &body[start + "roots = [".len()..];
    let end = list.find("];").expect("the roots list is closed");
    list[..end]
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix('"')?;
            rest.split_once('"').map(|(root, _)| root.to_string())
        })
        .collect()
}

#[test]
fn recipe_dir_is_inside_nix_source() {
    let roots = nix_source_roots();
    assert!(
        roots.iter().any(|root| root == "config"),
        "nix/source.nix must keep `config` as a root so config/recipes/ is built: {roots:?}"
    );
    assert!(
        !roots
            .iter()
            .any(|root| root == "recipes" || root.starts_with("recipes/")),
        "recipes belong under config/recipes/, never a top-level recipes/ root"
    );
    assert!(
        !repo_root().join("recipes").exists(),
        "a top-level recipes/ directory exists; recipes live under config/recipes/"
    );
}
