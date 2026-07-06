use crate::*;

pub(crate) fn cmd_fast_export(args: &[String]) -> Result<()> {
    if args != ["--all"] {
        return Err(GitError::Unsupported(
            "fast-export currently supports only --all".into(),
        ));
    }

    let git_dir = crate::session::cli_git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let store = FileRefStore::new(&git_dir, format);
    let mut refs = store.list_all_refs()?;
    refs.retain(|reference| reference.name.starts_with("refs/"));
    refs.sort_by(|left, right| left.name.cmp(&right.name));

    validate_fast_export_tag_refs(&db, &store, format, &refs)?;

    let mut exporter = FastExporter {
        db,
        format,
        next_mark: 1,
        commit_marks: HashMap::new(),
        blob_marks: HashMap::new(),
        out: io::BufWriter::new(io::stdout()),
    };

    for reference in refs {
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        let object = exporter.db.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            continue;
        }
        exporter.export_ref_history(&reference.name, oid)?;
    }
    exporter.out.flush()?;
    Ok(())
}

fn validate_fast_export_tag_refs(
    db: &FileObjectDatabase,
    store: &FileRefStore,
    format: ObjectFormat,
    refs: &[Ref],
) -> Result<()> {
    let mut severity = sley_fsck::content::SeverityConfig::new(true);
    severity.set("extraHeaderEntry", "error");
    severity.set("badGpgsig", "error");

    for reference in refs {
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Tag {
            continue;
        }
        let findings =
            sley_fsck::content::check_object_content(ObjectType::Tag, &object.body, &severity);
        if findings.is_empty() {
            let tag = Tag::parse_ref(format, &object.body)?;
            let read_oid = apply_replace_object(store, &tag.object)?;
            let target = match db.read_object(&read_oid) {
                Ok(target) => target,
                Err(_) => {
                    eprintln!("fatal: could not read tagged object '{}'", tag.object);
                    return Err(GitError::Exit(128));
                }
            };
            if target.object_type != tag.object_type {
                eprintln!(
                    "fatal: object '{}' tagged as '{}', but is a '{}' type",
                    tag.object,
                    tag.object_type.as_str(),
                    target.object_type.as_str()
                );
                return Err(GitError::Exit(128));
            }
            continue;
        }
        for finding in findings {
            let prefix = match finding.severity {
                sley_fsck::content::Severity::Error => "error in",
                sley_fsck::content::Severity::Warn => "warning in",
                sley_fsck::content::Severity::Ignore => continue,
            };
            if let Some(raw) = &finding.raw_stderr {
                eprintln!("error: {raw}");
            }
            eprintln!(
                "{prefix} tag {oid}: {}: {}",
                finding.msg_id.camel(),
                finding.detail
            );
        }
        return Err(GitError::Exit(128));
    }
    Ok(())
}

struct FastExporter {
    db: FileObjectDatabase,
    format: ObjectFormat,
    next_mark: u64,
    commit_marks: HashMap<ObjectId, u64>,
    blob_marks: HashMap<ObjectId, u64>,
    out: io::BufWriter<io::Stdout>,
}

impl FastExporter {
    fn export_ref_history(&mut self, ref_name: &str, oid: ObjectId) -> Result<()> {
        writeln!(self.out, "reset {ref_name}")?;
        self.export_commit_recursive(ref_name, oid)?;
        let mark = self
            .commit_marks
            .get(&oid)
            .copied()
            .ok_or_else(|| GitError::Command(format!("fast-export: missing mark for {oid}")))?;
        writeln!(self.out, "reset {ref_name}")?;
        writeln!(self.out, "from :{mark}")?;
        writeln!(self.out)?;
        Ok(())
    }

    fn export_commit_recursive(&mut self, ref_name: &str, oid: ObjectId) -> Result<u64> {
        if let Some(mark) = self.commit_marks.get(&oid).copied() {
            return Ok(mark);
        }
        let object = self.db.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::Command(format!(
                "fast-export: {oid} is not a commit"
            )));
        }
        let commit = Commit::parse(self.format, &object.body)?;
        for parent in &commit.parents {
            self.export_commit_recursive(ref_name, *parent)?;
        }
        self.ensure_tree_blob_marks(commit.tree)?;
        let mark = self.alloc_mark();
        self.commit_marks.insert(oid, mark);

        writeln!(self.out, "commit {ref_name}")?;
        writeln!(self.out, "mark :{mark}")?;
        self.out.write_all(b"author ")?;
        self.out.write_all(&commit.author)?;
        self.out.write_all(b"\ncommitter ")?;
        self.out.write_all(&commit.committer)?;
        self.out.write_all(b"\n")?;
        if let Some(encoding) = &commit.encoding {
            self.out.write_all(b"encoding ")?;
            self.out.write_all(encoding)?;
            self.out.write_all(b"\n")?;
        }
        writeln!(self.out, "data {}", commit.message.len())?;
        self.out.write_all(&commit.message)?;
        self.out.write_all(b"\n")?;
        if let Some(parent) = commit.parents.first() {
            let parent_mark = self.commit_marks.get(parent).copied().ok_or_else(|| {
                GitError::Command(format!("fast-export: missing parent mark for {parent}"))
            })?;
            writeln!(self.out, "from :{parent_mark}")?;
            for merge in commit.parents.iter().skip(1) {
                let merge_mark = self.commit_marks.get(merge).copied().ok_or_else(|| {
                    GitError::Command(format!("fast-export: missing merge mark for {merge}"))
                })?;
                writeln!(self.out, "merge :{merge_mark}")?;
            }
        }
        writeln!(self.out, "deleteall")?;
        self.export_tree_entries(commit.tree, Vec::new())?;
        writeln!(self.out)?;
        Ok(mark)
    }

    fn export_tree_entries(&mut self, tree_oid: ObjectId, prefix: Vec<u8>) -> Result<()> {
        let object = self.db.read_object(&tree_oid)?;
        if object.object_type != ObjectType::Tree {
            return Err(GitError::Command(format!(
                "fast-export: {tree_oid} is not a tree"
            )));
        }
        let entries = Tree::parse(self.format, &object.body)?.entries;
        for entry in entries {
            let path = join_export_path(&prefix, entry.name.as_bytes());
            match tree_entry_object_type(entry.mode) {
                ObjectType::Tree => self.export_tree_entries(entry.oid, path)?,
                ObjectType::Blob => {
                    let mark = self.blob_marks.get(&entry.oid).copied().ok_or_else(|| {
                        GitError::Command(format!(
                            "fast-export: missing blob mark for {}",
                            entry.oid
                        ))
                    })?;
                    write!(self.out, "M {:o} :{mark} ", entry.mode)?;
                    self.out.write_all(&path)?;
                    self.out.write_all(b"\n")?;
                }
                ObjectType::Commit => {
                    write!(self.out, "M 160000 {} ", entry.oid)?;
                    self.out.write_all(&path)?;
                    self.out.write_all(b"\n")?;
                }
                ObjectType::Tag => {}
            }
        }
        Ok(())
    }

    fn ensure_tree_blob_marks(&mut self, tree_oid: ObjectId) -> Result<()> {
        let object = self.db.read_object(&tree_oid)?;
        if object.object_type != ObjectType::Tree {
            return Err(GitError::Command(format!(
                "fast-export: {tree_oid} is not a tree"
            )));
        }
        let entries = Tree::parse(self.format, &object.body)?.entries;
        for entry in entries {
            match tree_entry_object_type(entry.mode) {
                ObjectType::Tree => self.ensure_tree_blob_marks(entry.oid)?,
                ObjectType::Blob => {
                    self.export_blob(entry.oid)?;
                }
                ObjectType::Commit | ObjectType::Tag => {}
            }
        }
        Ok(())
    }

    fn export_blob(&mut self, oid: ObjectId) -> Result<u64> {
        if let Some(mark) = self.blob_marks.get(&oid).copied() {
            return Ok(mark);
        }
        let object = self.db.read_object(&oid)?;
        if object.object_type != ObjectType::Blob {
            return Err(GitError::Command(format!(
                "fast-export: {oid} is not a blob"
            )));
        }
        let mark = self.alloc_mark();
        self.blob_marks.insert(oid, mark);
        writeln!(self.out, "blob")?;
        writeln!(self.out, "mark :{mark}")?;
        writeln!(self.out, "data {}", object.body.len())?;
        self.out.write_all(&object.body)?;
        self.out.write_all(b"\n")?;
        Ok(mark)
    }

    fn alloc_mark(&mut self) -> u64 {
        let mark = self.next_mark;
        self.next_mark += 1;
        mark
    }
}

fn join_export_path(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        return name.to_vec();
    }
    let mut out = Vec::with_capacity(prefix.len() + 1 + name.len());
    out.extend_from_slice(prefix);
    out.push(b'/');
    out.extend_from_slice(name);
    out
}
