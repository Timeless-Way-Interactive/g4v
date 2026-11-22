//! The pure decision behind Get Latest with binaries: given where the workspace is and what CI has built, where
//! should it go? Kept free of I/O so every row of the state table is a unit test.

use super::notes::{CommitNote, Note, NoteState};

/// Facts gathered about the workspace before deciding.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub head: String,
    /// First-parent history of `@{upstream}`, newest first, with notes.
    pub chain: Vec<CommitNote>,
    /// Commits on the branch that aren't on the server.
    pub ahead: u32,
    /// Why this is a programmer's workspace, if it is (local source edits, a submodule off its pin, …).
    pub programmer: Option<String>,
    /// The notes ref exists locally (CI has written at least one note).
    pub have_index: bool,
    /// Key of the installed build, when the install is complete.
    pub installed_key: Option<String>,
    /// Is `HEAD` an ancestor of the candidate? Is the candidate an ancestor of `HEAD`? Filled per candidate.
    pub head_is_ancestor_of: Vec<bool>,
    pub is_ancestor_of_head: Vec<bool>,
}

/// One ready build Get Latest could move to (newest first; later ones are fallbacks if a release is gone).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub commit: String,
    pub note: Note,
    /// How the workspace gets there.
    pub movement: Movement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Movement {
    /// Already there.
    Stay,
    /// Fast-forward.
    Forward,
    /// The workspace is ahead of the newest build (a clone at a tip that's still building): `reset --keep` back.
    Back,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Leave binaries alone and use the normal Get Latest, with the reason shown to the user.
    Normal(String),
    /// At the newest build, and it's installed.
    UpToDate,
    /// Move (or stay) and make sure the build is installed. Candidates are tried in order.
    Go(Vec<Candidate>),
    /// No commit on the branch has a usable build.
    NoBuild,
}

/// Newer commits than `commit` on the chain that are still waiting for a build.
pub fn waiting_above(chain: &[CommitNote], commit: &str) -> usize {
    chain.iter().take_while(|c| c.commit != commit).count()
}

/// Ready commits on the chain, newest first, as (index in chain, note).
pub fn ready(chain: &[CommitNote]) -> Vec<(usize, &CommitNote)> {
    chain.iter().enumerate().filter(|(_, c)| c.note.as_ref().is_some_and(Note::installable)).collect()
}

/// The builds Get Latest may try, newest first: the newest ready commit of each distinct key, at most `max` keys.
/// Content-only commits share their key, so without this a run of them pointing at one pruned release would use up
/// the whole fallback budget before an older build that still exists is ever tried.
pub fn candidates(chain: &[CommitNote], max: usize) -> Vec<(usize, &CommitNote)> {
    let mut seen = std::collections::HashSet::new();
    ready(chain).into_iter().filter(|(_, c)| seen.insert(c.note.as_ref().map(|n| n.key.clone()))).take(max).collect()
}

/// What the newest commits above the installable target are doing, for the toolbar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Newer {
    /// `count` newer commits, the newest of them building.
    Building { count: usize },
    /// The newest commit's build failed (or was given up on). Someone has to act.
    Failed { commit: String, count: usize },
    /// The newest commit has no build information yet: CI hasn't processed it (or never will).
    NotIndexed { commit: String, count: usize },
}

/// The state of what's newer than `target` on the chain, if anything is.
pub fn newer(chain: &[CommitNote], target: &str) -> Option<Newer> {
    let above: Vec<&CommitNote> = chain.iter().take_while(|c| c.commit != target).collect();
    let newest = above.first()?;
    let count = above.len();
    Some(match newest.note.as_ref().map(|n| n.state) {
        Some(NoteState::Building) | Some(NoteState::Superseded) | Some(NoteState::Ready) => Newer::Building { count },
        Some(NoteState::Failed) => Newer::Failed { commit: newest.commit.clone(), count },
        None => Newer::NotIndexed { commit: newest.commit.clone(), count },
    })
}

/// Is any commit on the chain building right now?
pub fn building(chain: &[CommitNote]) -> usize {
    chain.iter().filter(|c| c.note.as_ref().is_some_and(|n| n.state == NoteState::Building)).count()
}

/// Decides. `facts.head_is_ancestor_of[i]` / `is_ancestor_of_head[i]` describe the i-th entry of [`ready`].
pub fn decide(facts: &Facts) -> Plan {
    if let Some(why) = &facts.programmer {
        return Plan::Normal(why.clone());
    }
    if facts.ahead > 0 {
        return Plan::Normal("you have changes that aren't on the server yet".into());
    }
    if !facts.have_index {
        return Plan::Normal("the server has no build information yet".into());
    }
    let ready = candidates(&facts.chain, usize::MAX);
    let mut out = Vec::new();
    for (i, (_, c)) in ready.iter().enumerate() {
        let note = c.note.clone().expect("ready entries have notes");
        let movement = if c.commit == facts.head {
            Movement::Stay
        } else if facts.head_is_ancestor_of.get(i).copied().unwrap_or(false) {
            Movement::Forward
        } else if facts.is_ancestor_of_head.get(i).copied().unwrap_or(false) {
            Movement::Back
        } else {
            // Unrelated history; nothing sensible to do automatically.
            continue;
        };
        out.push(Candidate { commit: c.commit.clone(), note, movement });
    }
    match out.first() {
        None => Plan::NoBuild,
        Some(first) if first.movement == Movement::Stay && facts.installed_key.as_deref() == Some(first.note.key.as_str()) => {
            Plan::UpToDate
        }
        Some(_) => Plan::Go(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binaries::notes::AssetRef;

    fn note(state: NoteState, key: &str) -> Option<Note> {
        Some(Note {
            state,
            key: key.into(),
            release: format!("bin-{key}"),
            archive: Some(AssetRef { name: "a.zip".into(), sha256: None, bytes: None }),
            manifest: Some(AssetRef { name: "m.json".into(), sha256: None, bytes: None }),
        })
    }

    fn c(id: char, n: Option<Note>) -> CommitNote {
        CommitNote { commit: id.to_string().repeat(40), note: n }
    }

    fn id(ch: char) -> String {
        ch.to_string().repeat(40)
    }

    /// chain (newest first): d building, c ready k2, b no note, a ready k1
    fn base() -> Facts {
        Facts {
            head: id('a'),
            chain: vec![
                c('d', note(NoteState::Building, "k3")),
                c('c', note(NoteState::Ready, "k2")),
                c('b', None),
                c('a', note(NoteState::Ready, "k1")),
            ],
            have_index: true,
            installed_key: Some("k1".into()),
            head_is_ancestor_of: vec![true, false],
            is_ancestor_of_head: vec![false, true],
            ..Default::default()
        }
    }

    #[test]
    fn forward_to_newest_ready_not_the_tip() {
        let Plan::Go(c) = decide(&base()) else { panic!() };
        assert_eq!(c[0].commit, id('c'));
        assert_eq!(c[0].movement, Movement::Forward);
        assert_eq!(c[1].movement, Movement::Stay);
        assert_eq!(waiting_above(&base().chain, &id('c')), 1);
    }

    #[test]
    fn up_to_date_only_when_installed() {
        let mut f = base();
        f.head = id('c');
        f.head_is_ancestor_of = vec![false, false];
        f.is_ancestor_of_head = vec![false, true];
        f.installed_key = Some("k2".into());
        assert_eq!(decide(&f), Plan::UpToDate);
        f.installed_key = None; // fresh clone, or an install that never finished
        let Plan::Go(c) = decide(&f) else { panic!() };
        assert_eq!(c[0].movement, Movement::Stay);
    }

    #[test]
    fn back_from_a_tip_that_is_still_building() {
        let mut f = base();
        f.head = id('d');
        f.installed_key = None;
        f.head_is_ancestor_of = vec![false, false];
        f.is_ancestor_of_head = vec![true, true];
        let Plan::Go(c) = decide(&f) else { panic!() };
        assert_eq!((c[0].commit.as_str(), c[0].movement), (id('c').as_str(), Movement::Back));
    }

    #[test]
    fn programmers_unpushed_work_and_missing_index_use_normal_sync() {
        let mut f = base();
        f.programmer = Some("edits under Game/Source".into());
        assert!(matches!(decide(&f), Plan::Normal(_)));
        let mut f = base();
        f.ahead = 2;
        assert!(matches!(decide(&f), Plan::Normal(_)));
        let mut f = base();
        f.have_index = false;
        assert!(matches!(decide(&f), Plan::Normal(_)));
    }

    #[test]
    fn fallbacks_are_distinct_builds() {
        let chain = vec![
            c('e', note(NoteState::Ready, "k2")),
            c('d', note(NoteState::Ready, "k2")),
            c('c', note(NoteState::Ready, "k2")),
            c('b', note(NoteState::Failed, "k3")),
            c('a', note(NoteState::Ready, "k1")),
        ];
        let v: Vec<&str> = candidates(&chain, 8).iter().map(|(_, c)| c.note.as_ref().unwrap().key.as_str()).collect();
        assert_eq!(v, ["k2", "k1"], "three commits sharing a pruned k2 don't hide k1");
        assert_eq!(candidates(&chain, 1).len(), 1);
    }

    #[test]
    fn newer_reports_the_newest_commits_real_state() {
        let chain =
            vec![c('c', note(NoteState::Failed, "k3")), c('b', note(NoteState::Building, "k2")), c('a', note(NoteState::Ready, "k1"))];
        assert_eq!(newer(&chain, &id('a')), Some(Newer::Failed { commit: id('c'), count: 2 }));
        assert_eq!(newer(&chain[1..], &id('a')), Some(Newer::Building { count: 1 }));
        assert_eq!(newer(&[c('x', None), c('a', None)], &id('a')), Some(Newer::NotIndexed { commit: id('x'), count: 1 }));
        assert_eq!(newer(&chain, &id('c')), None);
    }

    #[test]
    fn nothing_ready() {
        let mut f = base();
        f.chain = vec![c('d', note(NoteState::Building, "k3")), c('a', note(NoteState::Failed, "k1"))];
        assert_eq!(decide(&f), Plan::NoBuild);
        assert_eq!(building(&f.chain), 1);
    }
}
