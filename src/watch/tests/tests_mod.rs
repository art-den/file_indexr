use crate::watch::*;

/// Non-existent index dir for tests that use fixed `/tmp` paths — none of
/// the fixture paths below are skipped with this context.
const TMP_INDEX: &str = "/tmp/file_indexr_unused_index";

fn make_event(kind: EventKind, paths: Vec<&Path>) -> Event {
    Event {
        kind,
        paths: paths.into_iter().map(|p| p.to_path_buf()).collect(),
        attrs: Default::default(),
    }
}

#[tokio::test]
async fn test_create_file_emits_modified() {
    let dir = std::env::temp_dir().join("file_indexr_watch_test");
    std::fs::create_dir_all(&dir).unwrap();
    let file_path = dir.join("new.txt");
    std::fs::write(&file_path, "hello").unwrap();

    let event = make_event(
        EventKind::Create(notify::event::CreateKind::File),
        vec![&file_path],
    );
    let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
    assert_eq!(changes.len(), 1);
    assert!(matches!(&changes[0], FileChange::Modified(p) if p == &file_path));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_modify_emits_modified() {
    let dir = std::env::temp_dir().join("file_indexr_watch_test2");
    std::fs::create_dir_all(&dir).unwrap();
    let file_path = dir.join("modified.txt");
    std::fs::write(&file_path, "hello").unwrap();

    let event = make_event(
        EventKind::Modify(notify::event::ModifyKind::Any),
        vec![&file_path],
    );
    let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
    assert_eq!(changes.len(), 1);
    assert!(matches!(&changes[0], FileChange::Modified(p) if p == &file_path));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_modify_dir_emits_nothing() {
    let dir = std::env::temp_dir().join("file_indexr_watch_test_modify_dir");
    std::fs::create_dir_all(&dir).unwrap();
    // A file inside the dir must NOT be picked up by a Modify on the dir.
    std::fs::write(dir.join("inner.txt"), "hello").unwrap();

    let event = make_event(
        EventKind::Modify(notify::event::ModifyKind::Metadata(
            notify::event::MetadataKind::Any,
        )),
        vec![&dir],
    );
    let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
    assert!(changes.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_modify_nonexistent_emits_nothing() {
    let path = std::env::temp_dir().join("file_indexr_watch_test_modify_missing");
    std::fs::remove_file(&path).ok();

    let event = make_event(
        EventKind::Modify(notify::event::ModifyKind::Any),
        vec![&path],
    );
    let changes = event_to_changes(
        &event,
        &std::env::temp_dir(),
        &std::env::temp_dir().join("file_indexr_unused_index"),
    )
    .await;
    assert!(changes.is_empty());
}

#[tokio::test]
async fn test_create_dir_emits_modified_for_files() {
    let dir = std::env::temp_dir().join("file_indexr_watch_test_create_dir_nonempty");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "hello").unwrap();
    std::fs::write(dir.join("b.txt"), "world").unwrap();

    let event = make_event(
        EventKind::Create(notify::event::CreateKind::Folder),
        vec![&dir],
    );
    let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
    // Create on a directory still walks it and emits Modified per file.
    let mut paths: Vec<_> = changes
        .iter()
        .filter_map(|c| match c {
            FileChange::Modified(p) => Some(p.clone()),
            _ => None,
        })
        .collect();
    paths.sort();
    assert_eq!(paths, vec![dir.join("a.txt"), dir.join("b.txt")]);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_remove_other_emits_deleted_dir() {
    let path = Path::new("/tmp/deleted.txt");
    let event = make_event(
        EventKind::Remove(notify::event::RemoveKind::Other),
        vec![path],
    );
    let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
    assert_eq!(changes.len(), 1);
    assert!(matches!(&changes[0], FileChange::DeletedDir(_)));
}

#[tokio::test]
async fn test_remove_file_emits_deleted() {
    let path = Path::new("/tmp/deleted.txt");
    let event = make_event(
        EventKind::Remove(notify::event::RemoveKind::File),
        vec![path],
    );
    let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
    assert_eq!(changes.len(), 1);
    assert!(matches!(&changes[0], FileChange::Deleted(_)));
}

#[tokio::test]
async fn test_remove_folder_emits_deleted_dir() {
    let path = Path::new("/tmp/deleted_dir");
    let event = make_event(
        EventKind::Remove(notify::event::RemoveKind::Folder),
        vec![path],
    );
    let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
    assert_eq!(changes.len(), 1);
    assert!(matches!(&changes[0], FileChange::DeletedDir(_)));
}

#[tokio::test]
async fn test_access_skipped() {
    let event = make_event(
        EventKind::Access(notify::event::AccessKind::Any),
        vec![Path::new("/tmp/file.txt")],
    );
    let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
    assert!(changes.is_empty());
}

#[tokio::test]
async fn test_rescan_flag_emits_rescan() {
    let event = Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
    let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
    assert_eq!(changes.len(), 1);
    assert!(matches!(&changes[0], FileChange::Rescan));
}

#[tokio::test]
async fn test_other_event_without_rescan_flag_emits_nothing() {
    let event = make_event(EventKind::Other, Vec::new());
    let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
    assert!(changes.is_empty());
}

#[tokio::test]
async fn test_directory_create_skipped() {
    let dir = std::env::temp_dir().join("file_indexr_watch_test3");
    std::fs::create_dir_all(&dir).unwrap();

    let event = make_event(
        EventKind::Create(notify::event::CreateKind::Folder),
        vec![&dir],
    );
    let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
    assert!(changes.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_rename_from_emits_deleted_and_deleted_dir() {
    // RenameMode::From: type is always unknown — emit both variants
    let from_path = Path::new("/tmp/old.txt");
    let event = make_event(
        EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::From,
        )),
        vec![from_path],
    );
    let changes = event_to_changes(&event, Path::new("/tmp"), Path::new(TMP_INDEX)).await;
    assert_eq!(changes.len(), 2);
    assert!(matches!(&changes[0], FileChange::Deleted(p) if p == &from_path.to_path_buf()));
    assert!(matches!(&changes[1], FileChange::DeletedDir(p) if p == &from_path.to_path_buf()));
}

#[tokio::test]
async fn test_rename_to_emits_modified() {
    // RenameMode::To: new path exists — should emit Modified
    let dir = std::env::temp_dir().join("file_indexr_rename_test");
    std::fs::create_dir_all(&dir).unwrap();
    let to_path = dir.join("new.txt");
    std::fs::write(&to_path, "renamed content").unwrap();

    let event = make_event(
        EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::To,
        )),
        vec![&to_path],
    );
    let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
    assert_eq!(changes.len(), 1);
    assert!(matches!(&changes[0], FileChange::Modified(p) if p == &to_path));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_rename_both_file_emits_delete_and_modified() {
    let dir = std::env::temp_dir().join("file_indexr_renameboth_file_test");
    std::fs::create_dir_all(&dir).unwrap();
    let from_path = dir.join("old_name.txt");
    std::fs::write(&from_path, "content").unwrap();
    let to_path = dir.join("new_name.txt");
    std::fs::write(&to_path, "content").unwrap();

    let event = make_event(
        EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Both,
        )),
        vec![&from_path, &to_path],
    );
    let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
    // Both delete variants are emitted (type is unknown), then Modified for the new path.
    assert_eq!(changes.len(), 3);
    assert!(matches!(&changes[0], FileChange::Deleted(p) if p == &from_path));
    assert!(matches!(&changes[1], FileChange::DeletedDir(p) if p == &from_path));
    assert!(matches!(&changes[2], FileChange::Modified(p) if p == &to_path));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_rename_both_dir_emits_deletedir_and_modified() {
    let dir = std::env::temp_dir().join("file_indexr_renameboth_dir_test");
    std::fs::create_dir_all(&dir).unwrap();
    let from_path = dir.join("old_dir");
    std::fs::create_dir_all(&from_path).unwrap();
    let to_path = dir.join("new_dir");
    std::fs::create_dir_all(&to_path).unwrap();
    // add_as_modified scans the 'to' dir for files — seed it so modified events are emitted.
    std::fs::write(to_path.join("inner.txt"), "data").unwrap();

    let event = make_event(
        EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Both,
        )),
        vec![&from_path, &to_path],
    );
    let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
    // Both delete variants are emitted (type is unknown), then Modified for the new path.
    assert_eq!(changes.len(), 3);
    assert!(matches!(&changes[0], FileChange::Deleted(p) if p == &from_path));
    assert!(matches!(&changes[1], FileChange::DeletedDir(p) if p == &from_path));
    assert!(matches!(&changes[2], FileChange::Modified(p) if p == &to_path.join("inner.txt")));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_rename_both_missing_parent_emits_both_deletes() {
    // The old path (and its parent) is gone — emit both Deleted and DeletedDir.
    let dir = std::env::temp_dir().join("file_indexr_renameboth_unk_test");
    std::fs::create_dir_all(&dir).unwrap();
    let from_path = Path::new("/nonexistent/parent/old.txt");
    let to_path = dir.join("actual_new.txt");
    std::fs::write(&to_path, "content").unwrap();

    let event = make_event(
        EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Both,
        )),
        vec![from_path, &to_path],
    );
    let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
    assert_eq!(changes.len(), 3);
    assert!(matches!(&changes[0], FileChange::Deleted(p) if p == &from_path.to_path_buf()));
    assert!(matches!(&changes[1], FileChange::DeletedDir(p) if p == &from_path.to_path_buf()));
    assert!(matches!(&changes[2], FileChange::Modified(p) if p == &to_path));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_create_hidden_dir_emits_nothing() {
    // A hidden directory as walk root is skipped entirely: its contents
    // are never indexed, so no walk and no changes.
    let dir = std::env::temp_dir().join("file_indexr_watch_hidden_root");
    let hidden = dir.join(".git");
    std::fs::create_dir_all(&hidden).unwrap();
    std::fs::write(hidden.join("config"), "data").unwrap();

    let event = make_event(
        EventKind::Create(notify::event::CreateKind::Folder),
        vec![&hidden],
    );
    let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
    assert!(changes.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_create_dir_under_hidden_emits_nothing() {
    // A walk root *under* a hidden directory is caught by the
    // `should_skip_path` branch of `skip_directory_walk` (as opposed to
    // the hidden-root case above, caught by the name check).
    let dir = std::env::temp_dir().join("file_indexr_watch_under_hidden");
    let objects = dir.join(".git").join("objects");
    std::fs::create_dir_all(&objects).unwrap();
    std::fs::write(objects.join("pack"), "data").unwrap();

    let event = make_event(
        EventKind::Create(notify::event::CreateKind::Folder),
        vec![&objects],
    );
    let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
    assert!(changes.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_create_dir_inside_index_emits_nothing() {
    // A folder created inside the index directory is skipped: no walk, no changes.
    let dir = std::env::temp_dir().join("file_indexr_watch_index_walk");
    let index = dir.join("file_indexr_data");
    let sub = index.join("segments");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("0.seg"), "data").unwrap();

    let event = make_event(
        EventKind::Create(notify::event::CreateKind::Folder),
        vec![&sub],
    );
    let changes = event_to_changes(&event, &dir, &index).await;
    assert!(changes.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_rename_both_to_hidden_dir_keeps_pairing() {
    // 'to' is a hidden directory: its walk is skipped, but the (from, to)
    // pair must stay intact — only the two deletes for 'from' are
    // emitted, and the live 'to' path must not be treated as renamed-from.
    let dir = std::env::temp_dir().join("file_indexr_watch_rename_hidden");
    let to_dir = dir.join(".cache");
    std::fs::create_dir_all(&to_dir).unwrap();
    std::fs::write(to_dir.join("blob.bin"), "data").unwrap();
    let from_path = dir.join("a.txt");

    let event = make_event(
        EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Both,
        )),
        vec![&from_path, &to_dir],
    );
    let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
    assert_eq!(changes.len(), 2);
    assert!(matches!(&changes[0], FileChange::Deleted(p) if p == &from_path));
    assert!(matches!(&changes[1], FileChange::DeletedDir(p) if p == &from_path));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_create_dot_file_emits_modified() {
    // Top-level dot files stay indexable — the hidden-dir-name check in
    // `skip_directory_walk` must not apply to regular files.
    let dir = std::env::temp_dir().join("file_indexr_watch_dot_file");
    std::fs::create_dir_all(&dir).unwrap();
    let file_path = dir.join(".env");
    std::fs::write(&file_path, "SECRET=1").unwrap();

    let event = make_event(
        EventKind::Create(notify::event::CreateKind::File),
        vec![&file_path],
    );
    let changes = event_to_changes(&event, &dir, &dir.join("index")).await;
    assert_eq!(changes.len(), 1);
    assert!(matches!(&changes[0], FileChange::Modified(p) if p == &file_path));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_should_skip_path_inside_index_dir() {
    // Non-hidden index dir: a hidden name (e.g. `.file_indexr`) would also be
    // skipped by the hidden-component rule, masking the index-prefix rule.
    let watched_dir = Path::new("/home/user/docs");
    let index_path = Path::new("/home/user/docs/file_indexr_data");
    // Anti-masking guard: if the fixture name ever becomes hidden, the
    // assertions below would no longer isolate the index-prefix rule.
    assert!(!has_hidden_component(
        Path::new("/home/user/docs/file_indexr_data/data.mfd"),
        watched_dir
    ));
    assert!(should_skip_path(
        Path::new("/home/user/docs/file_indexr_data/data.mfd"),
        watched_dir,
        index_path
    ));
    assert!(should_skip_path(
        Path::new("/home/user/docs/file_indexr_data"),
        watched_dir,
        index_path
    ));
}

#[test]
fn test_should_skip_path_outside_index_dir() {
    let watched_dir = Path::new("/home/user/docs");
    let index_path = Path::new("/home/user/docs/.file_indexr");
    assert!(!should_skip_path(
        Path::new("/home/user/docs/hello.txt"),
        watched_dir,
        index_path
    ));
    assert!(!should_skip_path(
        Path::new("/home/user/docs/sub/file.rs"),
        watched_dir,
        index_path
    ));
}

#[test]
fn test_should_skip_hidden_paths() {
    let watched_dir = Path::new("/home/user/docs");
    let index_path = Path::new("/home/user/docs/.file_indexr");
    assert!(should_skip_path(
        Path::new("/home/user/docs/.git/config"),
        watched_dir,
        index_path
    ));
    assert!(should_skip_path(
        Path::new("/home/user/docs/.venv/lib/site.py"),
        watched_dir,
        index_path
    ));
    assert!(should_skip_path(
        Path::new("/home/user/docs/src/.cache/data.bin"),
        watched_dir,
        index_path
    ));
}

#[tokio::test]
async fn test_collect_files_in_directory() {
    let dir = std::env::temp_dir().join("file_indexr_collect_test");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/b.txt"), "b").unwrap();
    // Hidden subdirectory should be skipped
    std::fs::create_dir_all(dir.join(".hidden")).unwrap();
    std::fs::write(dir.join(".hidden/c.txt"), "c").unwrap();

    let files = collect_files_in_directory(&dir, &dir.join("index")).await;
    let paths: Vec<_> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_str().unwrap())
        .collect();

    assert!(paths.contains(&"a.txt"));
    assert!(paths.contains(&"b.txt"));
    assert!(
        !paths.contains(&"c.txt"),
        "Hidden dir contents should be excluded"
    );
    assert_eq!(files.len(), 2);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_collect_files_empty_directory() {
    let dir = std::env::temp_dir().join("file_indexr_collect_empty");
    std::fs::create_dir_all(&dir).unwrap();

    let files = collect_files_in_directory(&dir, &dir.join("index")).await;
    assert!(files.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_collect_files_nonexistent_directory() {
    let files =
        collect_files_in_directory(Path::new("/nonexistent/path"), Path::new("/index")).await;
    assert!(files.is_empty());
}

#[tokio::test]
async fn test_collect_files_prunes_index_dir() {
    // An index-directory subdirectory is pruned during the walk.
    let dir = std::env::temp_dir().join("file_indexr_collect_index_prune");
    let index = dir.join("file_indexr_data");
    std::fs::create_dir_all(&index).unwrap();
    std::fs::write(index.join("0.seg"), "seg").unwrap();
    std::fs::write(dir.join("a.txt"), "a").unwrap();

    let files = collect_files_in_directory(&dir, &index).await;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0], dir.join("a.txt"));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_run_event_loop_propagates_notify_error() {
    let (bridge_tx, bridge_rx) = mpsc::unbounded_channel::<Result<Event, notify::Error>>();
    let (tx, _rx) = mpsc::channel::<FileChange>(16);
    let thread = std::thread::spawn(|| ());

    bridge_tx
        .send(Err(notify::Error::generic("test error")))
        .unwrap();
    drop(bridge_tx);

    let result = run_event_loop(
        bridge_rx,
        thread,
        PathBuf::from("/"),
        PathBuf::from("/index"),
        tx,
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_run_event_loop_clean_exit_is_ok() {
    let (bridge_tx, bridge_rx) = mpsc::unbounded_channel::<Result<Event, notify::Error>>();
    let (tx, _rx) = mpsc::channel::<FileChange>(16);
    let thread = std::thread::spawn(|| ());

    // Closing the bridge without an error is a clean exit.
    drop(bridge_tx);

    let result = run_event_loop(
        bridge_rx,
        thread,
        PathBuf::from("/"),
        PathBuf::from("/index"),
        tx,
    )
    .await;
    assert!(result.is_ok());
}
