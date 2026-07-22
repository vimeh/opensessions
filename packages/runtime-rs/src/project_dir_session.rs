use std::collections::{BTreeMap, BTreeSet};

pub type DirSessionMap = BTreeMap<String, Vec<String>>;

pub fn build_dir_session_map(
    sessions: impl IntoIterator<Item = (String, String)>,
) -> DirSessionMap {
    let mut map = BTreeMap::new();
    for (name, dir) in sessions {
        if dir.is_empty() {
            continue;
        }
        let names: &mut Vec<String> = map.entry(dir).or_default();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    map
}

pub fn resolve_session_for_project_dir(
    project_dir: &str,
    dir_session_map: &DirSessionMap,
) -> Option<String> {
    let exact_matches = exact_matches(project_dir, dir_session_map);
    if !exact_matches.is_empty() {
        return unique_match(exact_matches);
    }

    let related_matches = related_matches(project_dir, dir_session_map);
    if !related_matches.is_empty() {
        return unique_match(related_matches);
    }

    unique_match(encoded_matches(project_dir, dir_session_map)?)
}

/// True when the map has any candidate sessions for this project dir (exact,
/// prefix-related, or encoded). Callers holding several session sources
/// (e.g. local tmux vs remote providers) use this to scope resolution to the
/// first source that can claim the dir, so ambiguity inside one source is
/// not "rescued" — or worse, mis-resolved — by a coincidentally identical
/// path from another machine.
pub fn has_candidates_for_project_dir(
    project_dir: &str,
    dir_session_map: &DirSessionMap,
) -> bool {
    !exact_matches(project_dir, dir_session_map).is_empty()
        || !related_matches(project_dir, dir_session_map).is_empty()
        || encoded_matches(project_dir, dir_session_map).is_some_and(|matches| !matches.is_empty())
}

fn exact_matches(project_dir: &str, dir_session_map: &DirSessionMap) -> BTreeSet<String> {
    dir_session_map
        .get(project_dir)
        .map(|sessions| sessions.iter().cloned().collect::<BTreeSet<_>>())
        .unwrap_or_default()
}

fn related_matches(project_dir: &str, dir_session_map: &DirSessionMap) -> BTreeSet<String> {
    let mut matches = BTreeSet::new();
    for (dir, sessions) in dir_session_map {
        if !project_dir.starts_with(&format!("{dir}/"))
            && !dir.starts_with(&format!("{project_dir}/"))
        {
            continue;
        }
        matches.extend(sessions.iter().cloned());
    }
    matches
}

fn encoded_matches(
    project_dir: &str,
    dir_session_map: &DirSessionMap,
) -> Option<BTreeSet<String>> {
    let encoded = project_dir.strip_prefix("__encoded__:")?;

    let mut matches = BTreeSet::new();
    for (dir, sessions) in dir_session_map {
        if encode_project_dir(dir) != encoded {
            continue;
        }
        matches.extend(sessions.iter().cloned());
    }
    Some(matches)
}

fn unique_match(matches: BTreeSet<String>) -> Option<String> {
    if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        None
    }
}

fn encode_project_dir(dir: &str) -> String {
    dir.chars()
        .map(|ch| {
            if matches!(ch, '/' | '.' | '_') {
                '-'
            } else {
                ch
            }
        })
        .collect()
}
