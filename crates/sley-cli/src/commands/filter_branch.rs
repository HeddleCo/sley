//! `git filter-branch` compatibility command.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as Proc;
use std::time::{SystemTime, UNIX_EPOCH};

use sley::{GitError, Result};

pub(crate) fn cmd_filter_branch(args: &[String]) -> Result<()> {
    let exe = env::current_exe().map_err(|err| {
        GitError::Command(format!(
            "cannot locate current executable for filter-branch: {err}"
        ))
    })?;
    let helper = FilterBranchHelper::create(&exe)?;
    let mut command = Proc::new("sh");
    command.arg(&helper.script);
    command.args(args);
    command.env("PATH", helper.path_with_shim()?);
    command.env("SLEY_FILTER_BRANCH_SELF", &exe);
    let status = command.status().map_err(|err| {
        GitError::Command(format!("cannot run filter-branch shell engine: {err}"))
    })?;
    helper.cleanup();
    if status.success() {
        Ok(())
    } else {
        Err(GitError::Exit(status.code().unwrap_or(1)))
    }
}

struct FilterBranchHelper {
    root: PathBuf,
    bin: PathBuf,
    script: PathBuf,
}

impl FilterBranchHelper {
    fn create(exe: &Path) -> Result<Self> {
        let root = unique_helper_dir();
        let bin = root.join("bin");
        fs::create_dir_all(&bin).map_err(|err| {
            GitError::Command(format!(
                "cannot create filter-branch helper directory {}: {err}",
                bin.display()
            ))
        })?;
        let script = root.join("filter-branch.sh");
        write_executable(&script, FILTER_BRANCH_SCRIPT.as_bytes())?;
        let git = bin.join("git");
        create_git_shim(exe, &git)?;
        Ok(Self { root, bin, script })
    }

    fn path_with_shim(&self) -> Result<String> {
        let old_path = env::var_os("PATH").unwrap_or_default();
        let mut parts = vec![self.bin.clone()];
        parts.extend(env::split_paths(&old_path));
        env::join_paths(parts)
            .map_err(|err| GitError::Command(format!("cannot build filter-branch PATH: {err}")))
            .map(|path| path.to_string_lossy().into_owned())
    }

    fn cleanup(&self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn unique_helper_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    env::temp_dir().join(format!("sley-filter-branch-{}-{nanos}", std::process::id()))
}

fn write_executable(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::File::create(path)
        .map_err(|err| GitError::Command(format!("cannot write {}: {err}", path.display())))?;
    file.write_all(bytes)
        .map_err(|err| GitError::Command(format!("cannot write {}: {err}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = file
            .metadata()
            .map_err(|err| GitError::Command(format!("cannot stat {}: {err}", path.display())))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)
            .map_err(|err| GitError::Command(format!("cannot chmod {}: {err}", path.display())))?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_git_shim(exe: &Path, git: &Path) -> Result<()> {
    std::os::unix::fs::symlink(exe, git).map_err(|err| {
        GitError::Command(format!(
            "cannot create filter-branch git shim {} -> {}: {err}",
            git.display(),
            exe.display()
        ))
    })
}

#[cfg(not(unix))]
fn create_git_shim(exe: &Path, git: &Path) -> Result<()> {
    let body = format!("@echo off\r\n\"{}\" %*\r\n", exe.display());
    write_executable(git, body.as_bytes())
}

const FILTER_BRANCH_SCRIPT: &str = r#"#!/bin/sh

functions=$(cat << 'EOF'
EMPTY_TREE=$(git hash-object -t tree /dev/null)

warn () {
	echo "$*" >&2
}

map()
{
	if test -r "$workdir/../map/$1"
	then
		cat "$workdir/../map/$1"
	else
		echo "$1"
	fi
}

skip_commit()
{
	shift
	while test -n "$1"
	do
		shift
		map "$1"
		shift
	done
}

git_commit_non_empty_tree()
{
	if test $# = 3 && test "$1" = $(git rev-parse "$3^{tree}")
	then
		map "$3"
	elif test $# = 1 && test "$1" = $EMPTY_TREE
	then
		:
	else
		git commit-tree "$@"
	fi
}

die()
{
	echo >&2
	echo "$*" >&2
	exit 1
}
EOF
)

eval "$functions"

die_with_status()
{
	status=$1
	shift
	echo "$*" >&2
	exit "$status"
}

finish_ident()
{
	eval "name=\${GIT_$1_NAME}"
	eval "email=\${GIT_$1_EMAIL}"
	case "$name" in
	'') eval "GIT_$1_NAME=\${email%%@*}" ;;
	esac
	eval "export GIT_$1_NAME"
	eval "export GIT_$1_EMAIL"
	eval "export GIT_$1_DATE"
}

parse_ident_line()
{
	role=$1
	line=$2
	date=${line##*> }
	email_part=${line%> *}
	email=${email_part##*<}
	name=${email_part% <*}
	eval "GIT_${role}_NAME=\$name"
	eval "GIT_${role}_EMAIL=\$email"
	eval "GIT_${role}_DATE=\$date"
}

set_ident()
{
	author_line=$(sed -n 's/^author //p' ../commit)
	committer_line=$(sed -n 's/^committer //p' ../commit)
	parse_ident_line AUTHOR "$author_line"
	parse_ident_line COMMITTER "$committer_line"
	finish_ident AUTHOR
	finish_ident COMMITTER
}

filter_setup=
filter_env=
filter_tree=
filter_index=
filter_parent=
filter_msg=cat
filter_commit=
filter_tag_name=
filter_subdir=
state_branch=
orig_namespace=refs/original/
force=
prune_empty=
remap_to_ancestor=
tempdir=.git-rewrite

while :
do
	case "$1" in
	--)
		shift
		break
		;;
	--force|-f)
		shift
		force=t
		continue
		;;
	--remap-to-ancestor)
		shift
		remap_to_ancestor=t
		continue
		;;
	--prune-empty)
		shift
		prune_empty=t
		continue
		;;
	-*)
		;;
	*)
		break
		;;
	esac

	ARG=$1
	test $# != 1 || die "usage: git filter-branch [--setup <command>] [--subdirectory-filter <directory>] [--env-filter <command>] [--tree-filter <command>] [--index-filter <command>] [--parent-filter <command>] [--msg-filter <command>] [--commit-filter <command>] [--tag-name-filter <command>] [--original <namespace>] [-d <directory>] [-f | --force] [--state-branch <branch>] [--] [<rev-list options>...]"
	shift
	OPTARG=$1
	shift

	case "$ARG" in
	-d)
		tempdir=$OPTARG
		;;
	--setup)
		filter_setup=$OPTARG
		;;
	--subdirectory-filter)
		filter_subdir=$OPTARG
		remap_to_ancestor=t
		;;
	--env-filter)
		filter_env=$OPTARG
		;;
	--tree-filter)
		filter_tree=$OPTARG
		;;
	--index-filter)
		filter_index=$OPTARG
		;;
	--parent-filter)
		filter_parent=$OPTARG
		;;
	--msg-filter)
		filter_msg=$OPTARG
		;;
	--commit-filter)
		filter_commit="$functions; $OPTARG"
		;;
	--tag-name-filter)
		filter_tag_name=$OPTARG
		;;
	--original)
		orig_namespace=$(expr "$OPTARG/" : '\(.*[^/]\)/*$')/
		;;
	--state-branch)
		state_branch=$OPTARG
		;;
	*)
		die "usage: git filter-branch [--setup <command>] [--subdirectory-filter <directory>] [--env-filter <command>] [--tree-filter <command>] [--index-filter <command>] [--parent-filter <command>] [--msg-filter <command>] [--commit-filter <command>] [--tag-name-filter <command>] [--original <namespace>] [-d <directory>] [-f | --force] [--state-branch <branch>] [--] [<rev-list options>...]"
		;;
	esac
done

case "$prune_empty,$filter_commit" in
,)
	filter_commit='git commit-tree "$@"'
	;;
t,)
	filter_commit="$functions;"' git_commit_non_empty_tree "$@"'
	;;
,*)
	;;
*)
	die "Cannot set --prune-empty and --commit-filter at the same time"
	;;
esac

case "$force" in
t)
	rm -rf "$tempdir"
	;;
'')
	test -d "$tempdir" && die "$tempdir already exists, please remove it"
	;;
esac

orig_dir=$(pwd)
mkdir -p "$tempdir/t" || die ""
tempdir=$(cd "$tempdir" && pwd) || die ""
cd "$tempdir/t" || die ""
workdir=$(pwd) || die ""
trap 'cd "$orig_dir"; rm -rf "$tempdir"' 0

ORIG_GIT_DIR=$GIT_DIR
ORIG_GIT_WORK_TREE=$GIT_WORK_TREE
ORIG_GIT_INDEX_FILE=$GIT_INDEX_FILE
ORIG_GIT_AUTHOR_NAME=$GIT_AUTHOR_NAME
ORIG_GIT_AUTHOR_EMAIL=$GIT_AUTHOR_EMAIL
ORIG_GIT_AUTHOR_DATE=$GIT_AUTHOR_DATE
ORIG_GIT_COMMITTER_NAME=$GIT_COMMITTER_NAME
ORIG_GIT_COMMITTER_EMAIL=$GIT_COMMITTER_EMAIL
ORIG_GIT_COMMITTER_DATE=$GIT_COMMITTER_DATE

case "$GIT_DIR" in
'')
	GIT_DIR=$(git -C "$orig_dir" rev-parse --git-dir) || exit
	case "$GIT_DIR" in
	/*) ;;
	*) GIT_DIR=$orig_dir/$GIT_DIR ;;
	esac
	;;
esac
GIT_WORK_TREE=.
export GIT_DIR GIT_WORK_TREE

git for-each-ref > "$tempdir/backup-refs" || exit
while read sha1 type name
do
	case "$force,$name" in
	,$orig_namespace*)
		die "Cannot create a new backup.
A previous backup already exists in $orig_namespace
Force overwriting the backup with -f"
		;;
	t,$orig_namespace*)
		git update-ref -d "$name" "$sha1"
		;;
	esac
done < "$tempdir/backup-refs"

resolve_filter_branch_heads()
{
	if test $# = 0
	then
		git symbolic-ref -q HEAD || echo HEAD
		return
	fi
	while test $# != 0
	do
		arg=$1
		shift
		case "$arg" in
		--)
			break
			;;
		--all)
			git for-each-ref --format='%(refname)'
			;;
		--*)
			;;
		^*)
			;;
		*..*)
			right=${arg#*..}
			test -n "$right" || right=HEAD
			git rev-parse --symbolic-full-name "$right" 2>/dev/null || :
			;;
		*)
			git rev-parse --symbolic-full-name "$arg" 2>/dev/null || :
			;;
		esac
	done
}

resolve_filter_branch_heads "$@" > "$tempdir/raw-refs" || exit
while read ref
do
	case "$ref" in ^?*) continue ;; esac
	if git rev-parse --verify "$ref^0" >/dev/null 2>&1
	then
		echo "$ref"
	else
		warn "WARNING: not rewriting '$ref' (not a committish)"
	fi
done > "$tempdir/heads" < "$tempdir/raw-refs"

test -s "$tempdir/heads" || die "You must specify a ref to rewrite."

GIT_INDEX_FILE=$(pwd)/../index
export GIT_INDEX_FILE

mkdir ../map || die "Could not create map/ directory"

state_commit=
if test -n "$state_branch"
then
	state_commit=$(git rev-parse "$state_branch" 2>/dev/null || :)
	if test -n "$state_commit"
	then
		echo "Populating map from $state_branch ($state_commit)" >&2
		git show "$state_commit:filter.map" > "$tempdir/filter-map" ||
			die "Unable to load state from $state_branch:filter.map"
		while read line
		do
			case "$line" in
			*:*)
				echo "${line%:*}" > "../map/${line#*:}"
				;;
			*)
				die "Unable to load state from $state_branch:filter.map"
				;;
			esac
		done < "$tempdir/filter-map"
	else
		echo "Branch $state_branch does not exist. Will create" >&2
	fi
fi

dashdash=--
for arg
do
	if test "$arg" = --
	then
		dashdash=
		remap_to_ancestor=t
	fi
done
if test -n "$dashdash"
then
	dashdash=--
else
	remap_to_ancestor=t
fi

if test -n "$filter_subdir"
then
	set -- "$@" $dashdash "$filter_subdir"
fi

if test $# = 0
then
	set -- HEAD
fi

git rev-list --reverse --topo-order --parents --simplify-merges "$@" > ../revs ||
	die "Could not get the commits"
commits=$(wc -l < ../revs | tr -d " ")
test "$commits" -eq 0 && die_with_status 2 "Found nothing to rewrite"

if test -n "$filter_index" || test -n "$filter_tree" || test -n "$filter_subdir"
then
	need_index=t
else
	need_index=
fi

eval "$filter_setup" < /dev/null ||
	die "filter setup failed: $filter_setup"

git_filter_branch__commit_count=0
while read commit parents
do
	git_filter_branch__commit_count=$(($git_filter_branch__commit_count + 1))
	printf "\rRewrite %s (%s/%s)    " "$commit" "$git_filter_branch__commit_count" "$commits" >&2
	test -f "$workdir/../map/$commit" && continue

	case "$filter_subdir" in
	'')
		if test -n "$need_index"
		then
			GIT_ALLOW_NULL_SHA1=1 git read-tree -i -m "$commit"
		fi
		;;
	*)
		err=$(GIT_ALLOW_NULL_SHA1=1 git read-tree -i -m "$commit:$filter_subdir" 2>&1) || {
			if ! git rev-parse -q --verify "$commit:$filter_subdir" >/dev/null
			then
				rm -f "$GIT_INDEX_FILE"
			else
				echo >&2 "$err"
				false
			fi
		}
		;;
	esac || die "Could not initialize the index"

	GIT_COMMIT=$commit
	export GIT_COMMIT
	git cat-file commit "$commit" > ../commit ||
		die "Cannot read commit $commit"

	set_ident ||
		die "setting author/committer failed for commit $commit"
	eval "$filter_env" < /dev/null ||
		die "env filter failed: $filter_env"

	if test -n "$filter_tree"
	then
		git checkout-index -f -u -a ||
			die "Could not checkout the index"
		git clean -d -q -f -x
		eval "$filter_tree" < /dev/null ||
			die "tree filter failed: $filter_tree"
		(
			git diff-index --name-only "$commit" -- &&
			git ls-files --others
		) > "$tempdir/tree-state" || exit
		git update-index --add --replace --remove --stdin < "$tempdir/tree-state" || exit
	fi

	eval "$filter_index" < /dev/null ||
		die "index filter failed: $filter_index"

	parentstr=
	for parent in $parents
	do
		for reparent in $(map "$parent")
		do
			case "$parentstr " in
			*" -p $reparent "*)
				;;
			*)
				parentstr="$parentstr -p $reparent"
				;;
			esac
		done
	done
	if test -n "$filter_parent"
	then
		parentstr=$(echo "$parentstr" | eval "$filter_parent") ||
			die "parent filter failed: $filter_parent"
	fi

	{
		while IFS='' read -r header_line && test -n "$header_line"
		do
			:
		done
		cat
	} < ../commit | eval "$filter_msg" > ../message ||
		die "msg filter failed: $filter_msg"

	if test -n "$need_index"
	then
		tree=$(git write-tree)
	else
		tree=$(git rev-parse "$commit^{tree}")
	fi
	workdir=$workdir sh -c "$filter_commit" "git commit-tree" "$tree" $parentstr < ../message > "../map/$commit" ||
		die "could not write rewritten commit"
done < ../revs

if test "$remap_to_ancestor" = t
then
	while read ref
	do
		sha1=$(git rev-parse "$ref^0")
		test -f "$workdir/../map/$sha1" && continue
		ancestor=$(git rev-list --simplify-merges -1 "$ref" "$@")
		test "$ancestor" && echo $(map "$ancestor") > "$workdir/../map/$sha1"
	done < "$tempdir/heads"
fi

echo
while read ref
do
	test -f "$orig_namespace$ref" && continue
	sha1=$(git rev-parse "$ref^0")
	rewritten=$(map "$sha1")
	if test "$sha1" = "$rewritten"
	then
		warn "WARNING: Ref '$ref' is unchanged"
		continue
	fi
	case "$rewritten" in
	'')
		echo "Ref '$ref' was deleted"
		git update-ref -m "filter-branch: delete" -d "$ref" "$sha1" ||
			die "Could not delete $ref"
		;;
	*)
		echo "Ref '$ref' was rewritten"
		git update-ref -m "filter-branch: rewrite" "$ref" "$rewritten" "$sha1" 2>/dev/null ||
			die "Could not rewrite $ref"
		;;
	esac
	git update-ref -m "filter-branch: backup" "$orig_namespace$ref" "$sha1" ||
		exit
done < "$tempdir/heads"

if test -n "$filter_tag_name"
then
	git for-each-ref --format='%(objectname) %(objecttype) %(refname)' refs/tags |
	while read sha1 type ref
	do
		ref=${ref#refs/tags/}
		if test "$type" != commit && test "$type" != tag
		then
			continue
		fi
		if test "$type" = tag
		then
			sha1t=$sha1
			sha1=$(git rev-parse -q "$sha1^{commit}") || continue
		fi
		test -f "../map/$sha1" || continue
		new_sha1=$(cat "../map/$sha1")
		GIT_COMMIT=$sha1
		export GIT_COMMIT
		new_ref=$(echo "$ref" | eval "$filter_tag_name") ||
			die "tag name filter failed: $filter_tag_name"
		echo "$ref -> $new_ref ($sha1 -> $new_sha1)"
		if test "$type" = tag
		then
			new_sha1=$(
				(
					printf 'object %s\ntype commit\ntag %s\n' "$new_sha1" "$new_ref"
					git cat-file tag "$ref" |
					sed -n \
						-e '1,/^$/{
						  /^object /d
						  /^type /d
						  /^tag /d
						}' \
						-e '/^-----BEGIN PGP SIGNATURE-----/q' \
						-e 'p'
				) | git hash-object -t tag -w --stdin
			) || die "Could not create new tag object for $ref"
			if git cat-file tag "$ref" | grep '^-----BEGIN PGP SIGNATURE-----' >/dev/null 2>&1
			then
				warn "gpg signature stripped from tag object $sha1t"
			fi
		fi
		git update-ref "refs/tags/$new_ref" "$new_sha1" ||
			die "Could not write tag $new_ref"
	done
fi

unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
unset GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_AUTHOR_DATE
unset GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL GIT_COMMITTER_DATE
test -z "$ORIG_GIT_DIR" || { GIT_DIR=$ORIG_GIT_DIR && export GIT_DIR; }
test -z "$ORIG_GIT_WORK_TREE" || { GIT_WORK_TREE=$ORIG_GIT_WORK_TREE && export GIT_WORK_TREE; }
test -z "$ORIG_GIT_INDEX_FILE" || { GIT_INDEX_FILE=$ORIG_GIT_INDEX_FILE && export GIT_INDEX_FILE; }
test -z "$ORIG_GIT_AUTHOR_NAME" || { GIT_AUTHOR_NAME=$ORIG_GIT_AUTHOR_NAME && export GIT_AUTHOR_NAME; }
test -z "$ORIG_GIT_AUTHOR_EMAIL" || { GIT_AUTHOR_EMAIL=$ORIG_GIT_AUTHOR_EMAIL && export GIT_AUTHOR_EMAIL; }
test -z "$ORIG_GIT_AUTHOR_DATE" || { GIT_AUTHOR_DATE=$ORIG_GIT_AUTHOR_DATE && export GIT_AUTHOR_DATE; }
test -z "$ORIG_GIT_COMMITTER_NAME" || { GIT_COMMITTER_NAME=$ORIG_GIT_COMMITTER_NAME && export GIT_COMMITTER_NAME; }
test -z "$ORIG_GIT_COMMITTER_EMAIL" || { GIT_COMMITTER_EMAIL=$ORIG_GIT_COMMITTER_EMAIL && export GIT_COMMITTER_EMAIL; }
test -z "$ORIG_GIT_COMMITTER_DATE" || { GIT_COMMITTER_DATE=$ORIG_GIT_COMMITTER_DATE && export GIT_COMMITTER_DATE; }

if test -n "$state_branch"
then
	echo "Saving rewrite state to $state_branch" >&2
	state_blob=$(
		for file in ../map/*
		do
			from_commit=$(basename "$file")
			to_commit=$(cat "$file")
			echo "$from_commit:$to_commit"
		done | git hash-object -w --stdin
	) || die "Unable to save state"
	state_tree=$(printf '100644 blob %s\tfilter.map\n' "$state_blob" | git mktree)
	if test -n "$state_commit"
	then
		state_commit=$(echo "Sync" | git commit-tree "$state_tree" -p "$state_commit")
	else
		state_commit=$(echo "Sync" | git commit-tree "$state_tree")
	fi
	git update-ref "$state_branch" "$state_commit"
fi

cd "$orig_dir"
rm -rf "$tempdir"
trap - 0
git read-tree -u -m HEAD >/dev/null 2>&1 || :
exit 0
"#;
