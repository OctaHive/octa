use std::{fs, path::PathBuf};

use tempfile::TempDir;

use super::*;

fn write_octafile(directory: &Path, content: &str) -> PathBuf {
  fs::create_dir_all(directory).unwrap();
  let path = directory.join("Octafile.yml");
  fs::write(&path, content).unwrap();
  path
}

#[test]
fn discovers_single_and_recursive_roots_while_pruning_excludes() {
  let workspace = TempDir::new().unwrap();
  let root = write_octafile(
    workspace.path(),
    r#"
version: 1
monorepo:
  roots:
    - packages/*
    - services/**
  exclude:
    - target
    - node_modules
    - services/private/**
  max_depth: 3
tasks:
  annotated: !custom root
"#,
  );
  let api = write_octafile(&workspace.path().join("packages/api"), "version: 1\n");
  write_octafile(&workspace.path().join("packages/team/deep"), "version: 1\n");
  let auth = write_octafile(&workspace.path().join("services/auth"), "version: 1\n");
  let worker = write_octafile(&workspace.path().join("services/group/worker"), "version: 1\n");
  write_octafile(&workspace.path().join("services/one/two/three/four"), "version: 1\n");
  write_octafile(&workspace.path().join("services/target/generated"), "version: 1\n");
  write_octafile(&workspace.path().join("services/private/internal"), "version: 1\n");
  write_octafile(
    &workspace.path().join("services/auth/node_modules/package"),
    "version: 1\n",
  );

  let state = workspace.path().join(".octa/monorepo");
  let resolution = resolve(&api, api.parent().unwrap(), false, &state).unwrap();

  assert_eq!(resolution.root_octafile, root.canonicalize().unwrap());
  assert_eq!(
    resolution.current_namespace,
    Some(vec!["packages".into(), "api".into()])
  );
  assert_eq!(
    resolution.projects,
    vec![
      MonorepoProject {
        namespace: vec!["packages".into(), "api".into()],
        octafile: api.canonicalize().unwrap(),
      },
      MonorepoProject {
        namespace: vec!["services".into(), "auth".into()],
        octafile: auth.canonicalize().unwrap(),
      },
      MonorepoProject {
        namespace: vec!["services".into(), "group".into(), "worker".into()],
        octafile: worker.canonicalize().unwrap(),
      },
    ]
  );
}

#[test]
fn reuses_invalidates_and_clears_the_discovery_cache() {
  let workspace = TempDir::new().unwrap();
  let root = write_octafile(workspace.path(), "version: 1\nmonorepo:\n  roots: [packages/**]\n");
  let api_dir = workspace.path().join("packages/api");
  write_octafile(&api_dir, "version: 1\n");
  let state = workspace.path().join(".octa/monorepo");

  let first = resolve(&root, workspace.path(), false, &state).unwrap();
  let second = resolve(&root, workspace.path(), false, &state).unwrap();
  assert!(!first.cache_hit);
  assert!(second.cache_hit);
  assert_eq!(second.projects.len(), 1);
  assert!(state.is_dir());

  let database = sled::open(&state).unwrap();
  let tree = database.open_tree("monorepo_discovery_v1").unwrap();
  tree
    .insert(root.canonicalize().unwrap().as_os_str().as_encoded_bytes(), b"{")
    .unwrap();
  tree.flush().unwrap();
  drop(tree);
  drop(database);
  assert!(!resolve(&root, workspace.path(), false, &state).unwrap().cache_hit);
  assert!(resolve(&root, workspace.path(), false, &state).unwrap().cache_hit);

  clear_cache(&state).unwrap();
  assert!(!resolve(&root, workspace.path(), false, &state).unwrap().cache_hit);
  assert!(resolve(&root, workspace.path(), false, &state).unwrap().cache_hit);

  fs::remove_dir_all(api_dir).unwrap();
  let invalidated = resolve(&root, workspace.path(), false, &state).unwrap();
  assert!(!invalidated.cache_hit);
  assert!(invalidated.projects.is_empty());

  write_octafile(&workspace.path().join("packages/web"), "version: 1\n");
  let added = resolve(&root, workspace.path(), false, &state).unwrap();
  assert!(!added.cache_hit);
  assert_eq!(added.projects[0].namespace, ["packages", "web"]);
}

#[test]
fn explicit_entries_do_not_activate_an_ancestor_monorepo() {
  let workspace = TempDir::new().unwrap();
  write_octafile(workspace.path(), "version: 1\nmonorepo:\n  roots: [packages/*]\n");
  let project = write_octafile(&workspace.path().join("packages/api"), "version: 1\n");

  let state = workspace.path().join(".octa/monorepo");
  let resolution = resolve(&project, project.parent().unwrap(), true, &state).unwrap();

  assert_eq!(resolution.root_octafile, project.canonicalize().unwrap());
  assert!(resolution.projects.is_empty());
  assert_eq!(resolution.current_namespace, None);
  assert!(!state.exists());
}

#[test]
fn unmatched_local_octafiles_are_not_replaced_by_the_monorepo_root() {
  let workspace = TempDir::new().unwrap();
  write_octafile(workspace.path(), "version: 1\nmonorepo:\n  roots: [packages/*]\n");
  let local = write_octafile(&workspace.path().join("tools/release"), "version: 1\n");

  let state = workspace.path().join(".octa/monorepo");
  let resolution = resolve(&local, local.parent().unwrap(), false, &state).unwrap();

  assert_eq!(resolution.root_octafile, local.canonicalize().unwrap());
  assert!(resolution.projects.is_empty());
  assert_eq!(resolution.current_namespace, None);
  // An ancestor monorepo was configured, so discovery state is expected even
  // though this unmatched local Octafile ultimately remains standalone.
  assert!(state.is_dir());
}

#[test]
fn nearest_nested_monorepo_wins() {
  let workspace = TempDir::new().unwrap();
  write_octafile(workspace.path(), "version: 1\nmonorepo:\n  roots: [groups/**]\n");
  let nested_root = write_octafile(
    &workspace.path().join("groups/backend"),
    "version: 1\nmonorepo:\n  roots: [services/*]\n",
  );
  let service = write_octafile(&workspace.path().join("groups/backend/services/api"), "version: 1\n");

  let state = workspace.path().join(".octa/monorepo");
  let resolution = resolve(&service, service.parent().unwrap(), false, &state).unwrap();

  assert_eq!(resolution.root_octafile, nested_root.canonicalize().unwrap());
  assert_eq!(resolution.projects.len(), 1);
  assert_eq!(resolution.projects[0].namespace, ["services", "api"]);
}

#[test]
fn rejects_patterns_outside_the_monorepo_root() {
  let workspace = TempDir::new().unwrap();
  let root = write_octafile(workspace.path(), "version: 1\nmonorepo:\n  roots: [../packages/*]\n");
  let state = workspace.path().join(".octa/monorepo");

  assert!(matches!(
    resolve(&root, workspace.path(), false, &state),
    Err(MonorepoError::InvalidPattern { .. })
  ));

  fs::write(&root, "version: 1\nmonorepo:\n  roots: ['packages/[']\n").unwrap();
  assert!(matches!(
    resolve(&root, workspace.path(), false, &state),
    Err(MonorepoError::InvalidPattern { .. })
  ));
}

#[test]
fn standalone_resolution_and_clean_do_not_create_state() {
  let workspace = TempDir::new().unwrap();
  let root = write_octafile(workspace.path(), "version: 1\ntasks: {}\n");
  let state = workspace.path().join(".octa/monorepo");

  let resolution = resolve(&root, workspace.path(), false, &state).unwrap();
  assert_eq!(resolution.root_octafile, root.canonicalize().unwrap());
  assert!(resolution.projects.is_empty());
  assert!(!state.exists());

  clear_cache(&state).unwrap();
  assert!(!state.exists());
}
