use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;
use zip::ZipArchive;

pub const APP_ID: &str = "io.github.skyarse.ArchiveSearch";
pub const HOST_ROOT: &str = "/run/host/root";
pub const PAGE_SIZE: i64 = 250;
pub const NEW: i64 = 0;
pub const ONLINE: i64 = 1;
pub const OFFLINE: i64 = 2;
pub const PARTIAL: i64 = 3;

const VIRTUAL: &[&str] = &[
    "autofs",
    "bpf",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fuse.portal",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "proc",
    "pstore",
    "rpc_pipefs",
    "securityfs",
    "sysfs",
    "tracefs",
];

#[derive(Debug)]
pub struct Source {
    pub id: i64,
    pub path: String,
    pub state: i64,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub archive: String,
    pub member: String,
    pub ordinal: usize,
    pub stamp: String,
    pub state: i64,
    pub detail: String,
}

pub fn text_path(path: &Path) -> Result<&str> {
    path.to_str().context("Путь не в UTF-8")
}

pub fn filesystem_root() -> Option<PathBuf> {
    Path::new("/.flatpak-info")
        .is_file()
        .then(|| PathBuf::from(HOST_ROOT))
}

fn split_path(path: &Path) -> VecDeque<OsString> {
    path.components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_owned()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect()
}

fn resolve(path: &Path, prefix: Option<&Path>) -> Result<(PathBuf, PathBuf)> {
    ensure!(path.is_absolute(), "Укажите абсолютный путь к папке");
    let root = prefix.unwrap_or(Path::new("/"));
    let mut pending = split_path(path);
    let mut relative = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        if part == ".." {
            relative.pop();
            continue;
        }
        let next = root.join(&relative).join(&part);
        if fs::symlink_metadata(&next)?.is_symlink() {
            links += 1;
            ensure!(links <= 40, "Цикл символических ссылок");
            let target = fs::read_link(next)?;
            if target.is_absolute() {
                relative.clear();
            }
            for part in split_path(&target).into_iter().rev() {
                pending.push_front(part);
            }
        } else {
            relative.push(part);
        }
    }
    Ok((Path::new("/").join(&relative), root.join(relative)))
}

#[derive(Debug)]
struct Mount {
    point: PathBuf,
    signature: String,
    kind: String,
}

fn mount_field(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn disk_uuids(prefix: Option<&Path>) -> HashMap<PathBuf, String> {
    let directory = Path::new("/dev/disk/by-uuid");
    let path = prefix.map_or_else(
        || directory.to_owned(),
        |root| root.join(directory.strip_prefix("/").unwrap()),
    );
    let Ok(entries) = fs::read_dir(path) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let uuid = entry.file_name().into_string().ok()?;
            let (_, device) = resolve(&directory.join(&uuid), prefix).ok()?;
            Some((device, uuid))
        })
        .collect()
}

fn read_mounts() -> Result<Vec<Mount>> {
    let table = fs::read_to_string("/proc/self/mountinfo")?;
    let prefix = filesystem_root();
    let devices = disk_uuids(prefix.as_deref());
    Ok(table
        .lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let mut right = right.split_whitespace();
            let mut left = left.split_whitespace().skip(3);
            let root = left.next()?;
            let point = left.next()?;
            let kind = right.next()?.to_owned();
            let source = right.next()?;
            let device = if source.starts_with("/dev/") && !devices.is_empty() {
                resolve(Path::new(&mount_field(source)), prefix.as_deref())
                    .ok()
                    .and_then(|(_, device)| devices.get(&device))
                    .map_or_else(|| source.to_owned(), |uuid| format!("UUID={uuid}"))
            } else {
                source.to_owned()
            };
            Some(Mount {
                point: PathBuf::from(mount_field(point)),
                signature: format!("{point} {root} {kind} {device}"),
                kind,
            })
        })
        .collect())
}

fn mount_for<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.point))
        .max_by_key(|mount| mount.point.components().count())
}

fn fingerprint(metadata: &Metadata) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec()
    )
}

fn fold(value: &str) -> String {
    value.nfc().case_fold().collect()
}

pub fn database(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(4))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(connection)
}

pub fn initialize(path: &Path) -> Result<Connection> {
    fs::create_dir_all(path.parent().context("Нет папки индекса")?)?;
    let db = database(path)?;
    db.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS sources (
            id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT UNIQUE NOT NULL,
            mount TEXT, state INTEGER NOT NULL DEFAULT 0, detail TEXT NOT NULL DEFAULT ''
         );
         CREATE TABLE IF NOT EXISTS archives (
            id INTEGER PRIMARY KEY, source INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
            path TEXT NOT NULL, stamp TEXT NOT NULL, error TEXT NOT NULL DEFAULT '',
            UNIQUE(source, path)
         );
         CREATE TABLE IF NOT EXISTS entries (
            id INTEGER PRIMARY KEY,
            archive INTEGER NOT NULL REFERENCES archives(id) ON DELETE CASCADE,
            ordinal INTEGER NOT NULL, path TEXT NOT NULL, folded TEXT NOT NULL,
            UNIQUE(archive, ordinal)
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS entry_search USING fts5(
            folded, content='entries', content_rowid='id', tokenize='trigram case_sensitive 1'
         );
         CREATE TRIGGER IF NOT EXISTS entry_insert AFTER INSERT ON entries BEGIN
            INSERT INTO entry_search(rowid,folded) VALUES(new.id,new.folded);
         END;
         CREATE TRIGGER IF NOT EXISTS entry_delete AFTER DELETE ON entries BEGIN
            INSERT INTO entry_search(entry_search,rowid,folded) VALUES('delete',old.id,old.folded);
         END;"
    )?;
    Ok(db)
}

pub fn add_source(db: &Connection, path: &str) -> Result<i64> {
    ensure!(
        Path::new(path).is_absolute(),
        "Укажите абсолютный путь к папке"
    );
    db.execute("INSERT OR IGNORE INTO sources(path) VALUES (?)", [path])?;
    Ok(
        db.query_row("SELECT id FROM sources WHERE path=?", [path], |row| {
            row.get(0)
        })?,
    )
}

pub fn sources(db: &Connection) -> Result<Vec<Source>> {
    let mut statement = db.prepare("SELECT id, path, state, detail FROM sources ORDER BY path")?;
    Ok(statement
        .query_map([], |row| {
            Ok(Source {
                id: row.get(0)?,
                path: row.get(1)?,
                state: row.get(2)?,
                detail: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn status(db: &Connection, id: i64, state: i64, detail: &str) -> Result<()> {
    db.execute(
        "UPDATE sources SET state=?, detail=? WHERE id=?",
        params![state, detail, id],
    )?;
    Ok(())
}

pub fn mark_unmounted(db: &Connection) -> Result<bool> {
    let mounts: HashSet<_> = read_mounts()?
        .into_iter()
        .map(|mount| mount.signature)
        .collect();
    let mut query =
        db.prepare("SELECT id, mount FROM sources WHERE mount IS NOT NULL AND state!=2")?;
    let rows = query
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut changed = false;
    for (id, mount) in rows {
        if !mounts.contains(&mount) {
            status(db, id, OFFLINE, "Накопитель или сетевая папка отключены")?;
            changed = true;
        }
    }
    Ok(changed)
}

pub fn search(db: &Connection, query: &str, offset: i64) -> Result<(Vec<Hit>, bool)> {
    let query = query.trim().replace('\\', "/");
    if query.is_empty() {
        return Ok((Vec::new(), false));
    }
    let folded = fold(&query);
    let (condition, needle) = if folded.chars().count() >= 3 {
        (
            "e.id IN (SELECT rowid FROM entry_search WHERE entry_search MATCH ?1)",
            format!(r#""{}""#, folded.replace('"', r#""""#)),
        )
    } else {
        ("instr(e.folded, ?1)>0", folded)
    };
    let mut statement = db.prepare_cached(&format!(
        "WITH matches AS (
            SELECT a.path AS archive, e.path AS member, e.ordinal, a.stamp,
                CASE WHEN a.error!='' AND s.state!=2 THEN 3 ELSE s.state END AS state,
                CASE WHEN s.state=2 OR a.error='' THEN s.detail ELSE a.error END AS detail,
                row_number() OVER (
                    PARTITION BY a.path, e.ordinal, e.path
                    ORDER BY CASE WHEN a.error!='' THEN 4 WHEN s.state=1 THEN 0
                                  WHEN s.state=3 THEN 1 ELSE 2 END, s.id
                ) AS rank
            FROM entries e JOIN archives a ON e.archive=a.id JOIN sources s ON a.source=s.id
            WHERE {condition}
        )
        SELECT archive, member, ordinal, stamp, state, detail FROM matches
        WHERE rank=1 ORDER BY archive, member, ordinal LIMIT ?2 OFFSET ?3"
    ))?;
    let mut hits = statement
        .query_map(params![needle, PAGE_SIZE + 1, offset], |row| {
            Ok(Hit {
                archive: row.get(0)?,
                member: row.get(1)?,
                ordinal: row.get(2)?,
                stamp: row.get(3)?,
                state: row.get(4)?,
                detail: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = hits.len() as i64 > PAGE_SIZE;
    hits.truncate(PAGE_SIZE as usize);
    Ok((hits, more))
}

fn read_entries(path: &Path) -> Result<(String, Vec<(usize, String, String)>)> {
    let file = File::open(path)?;
    let before = fingerprint(&file.metadata()?);
    let archive = ZipArchive::new(&file)?;
    let mut entries = Vec::with_capacity(archive.len());
    for (ordinal, name) in archive.file_names().enumerate() {
        if !name.ends_with(['/', '\\']) {
            let name = name.replace('\\', "/");
            entries.push((ordinal, name.clone(), fold(&name)));
        }
    }
    ensure!(
        before == fingerprint(&file.metadata()?) && before == fingerprint(&fs::metadata(path)?),
        "Архив изменился во время чтения"
    );
    Ok((before, entries))
}

fn index_archive(
    db: &mut Connection,
    source: i64,
    logical: &Path,
    physical: &Path,
) -> Result<bool> {
    let path = text_path(logical)?;
    let current = fingerprint(&fs::symlink_metadata(physical)?);
    let previous: Option<(i64, String, String)> = db
        .query_row(
            "SELECT id, stamp, error FROM archives WHERE source=? AND path=?",
            params![source, path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((_, stamp, error)) = &previous {
        if *stamp == current && error.is_empty() {
            return Ok(true);
        }
    }

    let (stamp, entries) = match read_entries(physical) {
        Ok(index) => index,
        Err(error) => {
            let message = format!("Архив недоступен для чтения: {error}");
            db.execute(
                "INSERT INTO archives(source, path, stamp, error) VALUES (?, ?, ?, ?)
                ON CONFLICT(source,path) DO UPDATE SET error=excluded.error",
                params![source, path, current, message],
            )?;
            return Ok(false);
        }
    };
    let transaction = db.transaction()?;
    let archive_id: i64 = transaction.query_row(
        "INSERT INTO archives(source, path, stamp) VALUES (?, ?, ?)
         ON CONFLICT(source,path) DO UPDATE SET stamp=excluded.stamp, error='' RETURNING id",
        params![source, path, stamp],
        |row| row.get(0),
    )?;
    transaction.execute("DELETE FROM entries WHERE archive=?", [archive_id])?;
    {
        let mut insert = transaction
            .prepare("INSERT INTO entries(archive, ordinal, path, folded) VALUES (?, ?, ?, ?)")?;
        for (ordinal, name, folded) in entries {
            insert.execute(params![archive_id, ordinal, name, folded])?;
        }
    }
    transaction.commit()?;
    Ok(true)
}

pub fn scan(
    db: &mut Connection,
    source: i64,
    prefix: Option<&Path>,
    heartbeat: &Path,
) -> Result<()> {
    let (path, saved_mount): (String, Option<String>) = db.query_row(
        "SELECT path, mount FROM sources WHERE id=?",
        [source],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mounts = read_mounts()?;
    if let Some(saved) = &saved_mount {
        if !mounts.iter().any(|mount| &mount.signature == saved) {
            return status(
                db,
                source,
                OFFLINE,
                "Накопитель или сетевая папка отключены",
            );
        }
    }
    let root = resolve(Path::new(&path), prefix);
    let (logical, physical) = match root {
        Ok(paths) => paths,
        Err(error) => return status(db, source, OFFLINE, &format!("Папка недоступна: {error}")),
    };
    let root_meta = match fs::metadata(&physical) {
        Ok(meta) if meta.is_dir() => meta,
        _ => return status(db, source, OFFLINE, "Папка недоступна"),
    };
    let mount = mount_for(&mounts, &physical).context("Точка монтирования не найдена")?;
    if saved_mount
        .as_ref()
        .is_some_and(|saved| *saved != mount.signature)
    {
        return status(db, source, OFFLINE, "В этом пути теперь другой накопитель");
    }
    ensure!(
        !VIRTUAL.contains(&mount.kind.as_str()),
        "Выберите папку с файлами"
    );
    db.execute(
        "UPDATE sources SET mount=? WHERE id=?",
        params![mount.signature, source],
    )?;
    let excluded: HashSet<_> = mounts
        .iter()
        .filter(|child| child.point != mount.point && child.point.starts_with(&physical))
        .map(|child| &child.point)
        .collect();
    let mut stack = vec![(logical, physical.clone())];
    let mut seen = HashSet::new();
    let mut failures = 0;
    let mut bad_archives = 0;
    let mut scanned_root = false;
    fs::write(heartbeat, b"")?;
    let mut tick = Instant::now();

    while let Some((logical_dir, physical_dir)) = stack.pop() {
        let Ok(directory) = fs::read_dir(&physical_dir) else {
            failures += 1;
            continue;
        };
        scanned_root = true;
        for entry in directory {
            if tick.elapsed() >= Duration::from_secs(2) {
                fs::write(heartbeat, b"")?;
                tick = Instant::now();
            }
            let Ok(entry) = entry else {
                failures += 1;
                continue;
            };
            let physical = entry.path();
            if excluded.contains(&physical) {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                failures += 1;
                continue;
            };
            let logical = logical_dir.join(entry.file_name());
            if kind.is_dir() {
                stack.push((logical, physical));
            } else if kind.is_file()
                && physical
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
            {
                let Ok(name) = text_path(&logical) else {
                    failures += 1;
                    continue;
                };
                seen.insert(name.to_owned());
                match index_archive(db, source, &logical, &physical) {
                    Ok(true) => {}
                    Ok(false) => bad_archives += 1,
                    Err(_) => failures += 1,
                }
            }
        }
    }

    let still_mounted = mount_for(&read_mounts()?, &physical)
        .is_some_and(|current| current.signature == mount.signature);
    let same_root = fs::metadata(&physical)
        .is_ok_and(|meta| meta.dev() == root_meta.dev() && meta.ino() == root_meta.ino());
    if !scanned_root || !still_mounted || !same_root {
        return status(
            db,
            source,
            OFFLINE,
            "Папка стала недоступна во время сканирования",
        );
    }

    let transaction = db.transaction()?;
    if failures == 0 {
        let known = {
            let mut entries = transaction.prepare("SELECT id,path FROM archives WHERE source=?")?;
            entries
                .query_map([source], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut delete = transaction.prepare("DELETE FROM archives WHERE id=?")?;
        for (id, path) in known {
            if !seen.contains(&path) {
                delete.execute([id])?;
            }
        }
    }
    let (state, detail) = if failures > 0 {
        (
            PARTIAL,
            format!("Недоступных путей: {failures}. Архивов с ошибками: {bad_archives}"),
        )
    } else {
        (
            ONLINE,
            if bad_archives == 0 {
                String::new()
            } else {
                format!("Архивов с ошибками: {bad_archives}")
            },
        )
    };
    status(&transaction, source, state, &detail)?;
    transaction.commit()?;
    Ok(())
}

pub fn member_basename(member: &str) -> &str {
    member
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty() && *name != "." && *name != ".." && !name.contains('\0'))
        .unwrap_or("file")
}

pub fn extract(
    archive: &Path,
    ordinal: usize,
    member: &str,
    stamp: &str,
    prefix: Option<&Path>,
    destination: &Path,
) -> Result<()> {
    let (_, archive) = resolve(archive, prefix)?;
    let file = File::open(&archive)?;
    ensure!(
        fingerprint(&file.metadata()?) == stamp,
        "Архив изменился. Обновите индекс"
    );
    let mut zip = ZipArchive::new(&file)?;
    let entry = match zip.by_index(ordinal) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::UnsupportedArchive(message)) => {
            bail!("Файл защищён паролем или использует неподдерживаемое сжатие: {message}")
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        entry.name().replace('\\', "/") == member && !entry.is_dir() && !entry.is_symlink(),
        "Выбранный элемент не является обычным файлом"
    );
    let size = entry.size();
    let mut output =
        tempfile::NamedTempFile::new_in(destination.parent().context("Нет папки назначения")?)?;
    let copied = io::copy(&mut entry.take(size.saturating_add(1)), &mut output)?;
    ensure!(copied == size, "Размер извлечённого файла не совпадает");
    ensure!(
        fingerprint(&file.metadata()?) == stamp && fingerprint(&fs::metadata(archive)?) == stamp,
        "Архив изменился во время извлечения"
    );
    output.flush()?;
    output.persist_noclobber(destination)?;
    Ok(())
}
