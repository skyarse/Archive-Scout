use anyhow::Result;
use archive_search::{
    OFFLINE, ONLINE, PARTIAL, add_source, database, extract, initialize, scan, search,
};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use tempfile::tempdir;
use zip::write::SimpleFileOptions;

fn write_zip(dest: &Path, files: &[(&str, &str)]) -> Result<()> {
    let mut zip = zip::ZipWriter::new(File::create(dest)?);
    for (path, content) in files {
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(*path, options)?;
        zip.write_all(content.as_bytes())?;
    }
    zip.finish()?;
    Ok(())
}

#[test]
fn searches_sources_updates_changes_and_retains_offline_results() -> Result<()> {
    let temp = tempdir()?;
    let local = temp.path().join("local");
    let remote = temp.path().join("server");
    let disconnected = temp.path().join("disconnected");
    fs::create_dir(&local)?;
    fs::create_dir(&remote)?;
    let first = local.join("one.ZIP");
    let second = remote.join("two.zip");
    write_zip(
        &first,
        &[("Папка/ОТЧЁТ.txt", "local"), ("other/%data.txt", "other")],
    )?;
    write_zip(&second, &[("Финансы/отчёт.txt", "remote")])?;

    let path = temp.path().join("index.sqlite3");
    let heartbeat = temp.path().join("heartbeat");
    let mut db = initialize(&path)?;
    let local_id = add_source(&db, local.to_str().unwrap())?;
    let remote_id = add_source(&db, remote.to_str().unwrap())?;
    scan(&mut db, local_id, None, &heartbeat)?;
    scan(&mut db, remote_id, None, &heartbeat)?;
    assert_eq!(search(&db, "отчёт", 0)?.0.len(), 2);
    assert_eq!(
        search(&db, "папка\\ОТЧЁТ", 0)?.0[0].archive,
        first.to_str().unwrap()
    );
    assert_eq!(search(&db, "%", 0)?.0.len(), 1);

    let hit = search(&db, "финансы", 0)?.0.remove(0);
    let output = temp.path().join("selected.txt");
    extract(
        Path::new(&hit.archive),
        hit.ordinal,
        &hit.member,
        &hit.stamp,
        None,
        &output,
    )?;
    assert_eq!(fs::read_to_string(&output)?, "remote");

    fs::rename(&remote, &disconnected)?;
    scan(&mut db, remote_id, None, &heartbeat)?;
    drop(db);
    db = database(&path)?;
    let states: Vec<_> = search(&db, "отчёт", 0)?
        .0
        .iter()
        .map(|hit| hit.state)
        .collect();
    assert_eq!(states, vec![ONLINE, OFFLINE]);

    fs::rename(&disconnected, &remote)?;
    write_zip(&second, &[("Архив/\"новый\".txt", "updated")])?;
    write_zip(
        &remote.join("created.zip"),
        &[("Папка/Новый файл.txt", "fresh")],
    )?;
    scan(&mut db, remote_id, None, &heartbeat)?;
    let hits = search(&db, "новый", 0)?.0;
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|hit| hit.state == ONLINE));
    assert_eq!(search(&db, "\"новый\"", 0)?.0.len(), 1);
    assert_eq!(search(&db, "отчёт", 0)?.0.len(), 1);
    let stale_output = temp.path().join("stale.txt");
    assert!(
        extract(
            Path::new(&hit.archive),
            hit.ordinal,
            &hit.member,
            &hit.stamp,
            None,
            &stale_output
        )
        .is_err()
    );
    assert!(!stale_output.exists());

    fs::write(&second, b"invalid zip")?;
    scan(&mut db, remote_id, None, &heartbeat)?;
    assert_eq!(search(&db, "\"новый\"", 0)?.0[0].state, PARTIAL);

    fs::remove_file(first)?;
    scan(&mut db, local_id, None, &heartbeat)?;
    assert!(search(&db, "отчёт", 0)?.0.is_empty());
    db.execute("DELETE FROM sources WHERE id=?", [remote_id])?;
    assert!(search(&db, "новый", 0)?.0.is_empty());
    Ok(())
}

#[test]
fn handles_overlapping_sources_and_extracts_only_selected_member() -> Result<()> {
    let temp = tempdir()?;
    let source = temp.path().join("source");
    let nested = source.join("nested");
    fs::create_dir_all(&nested)?;
    let archive = nested.join("test.zip");
    write_zip(
        &archive,
        &[
            ("../../outside.txt", "chosen"),
            ("sub/untouched.txt", "second"),
        ],
    )?;

    let mut db = initialize(&temp.path().join("index.sqlite3"))?;
    let heartbeat = temp.path().join("heartbeat");
    for source in [&source, &nested] {
        let id = add_source(&db, source.to_str().unwrap())?;
        scan(&mut db, id, None, &heartbeat)?;
    }
    let hits = search(&db, "outside", 0)?.0;
    assert_eq!(hits.len(), 1);
    let hit = &hits[0];
    let destination = temp
        .path()
        .join(archive_search::member_basename(&hit.member));
    extract(
        &archive,
        hit.ordinal,
        &hit.member,
        &hit.stamp,
        None,
        &destination,
    )?;
    assert_eq!(fs::read_to_string(&destination)?, "chosen");
    assert!(!temp.path().join("untouched.txt").exists());
    assert!(
        extract(
            &archive,
            hit.ordinal,
            &hit.member,
            &hit.stamp,
            None,
            &destination
        )
        .is_err()
    );
    assert_eq!(fs::read_to_string(destination)?, "chosen");
    Ok(())
}

#[test]
#[ignore = "requires a private mount namespace with permission to mount tmpfs"]
fn retains_index_after_real_unmount() -> Result<()> {
    use std::process::Command;

    let temp = tempdir()?;
    let point = temp.path().join("mounted");
    fs::create_dir(&point)?;
    assert!(
        Command::new("mount")
            .args(["-t", "tmpfs", "tmpfs"])
            .arg(&point)
            .status()?
            .success()
    );
    write_zip(
        &point.join("mounted.zip"),
        &[("monthly-report.txt", "from mount")],
    )?;
    let mut db = initialize(&temp.path().join("index.sqlite3"))?;
    let id = add_source(&db, point.to_str().unwrap())?;
    let heartbeat = temp.path().join("heartbeat");
    scan(&mut db, id, None, &heartbeat)?;
    assert_eq!(search(&db, "monthly", 0)?.0[0].state, ONLINE);

    assert!(Command::new("umount").arg(&point).status()?.success());
    assert!(point.is_dir());
    assert!(archive_search::mark_unmounted(&db)?);
    scan(&mut db, id, None, &heartbeat)?;
    assert_eq!(search(&db, "monthly", 0)?.0[0].state, OFFLINE);
    Ok(())
}
