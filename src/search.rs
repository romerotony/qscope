//! Fuzzy-match cached jobs field by field, without querying the scheduler.

use crate::scheduler::Job;
use nucleo_matcher::{
  Matcher, Utf32Str,
  pattern::{AtomKind, CaseMatching, Normalization, Pattern},
};

pub struct JobMatch {
  pub score: u32,
  /// Indices are grapheme positions, not UTF-8 byte offsets.
  pub indices: [Vec<u32>; 6],
}

pub struct Search {
  matcher: Matcher,
  buffer: Vec<char>,
}

impl Default for Search {
  fn default() -> Self {
    Self {
      matcher: Matcher::new(nucleo_matcher::Config::DEFAULT),
      buffer: Vec::new(),
    }
  }
}

impl Search {
  pub fn rank(&mut self, jobs: &[Job], query: &str) -> Vec<Option<JobMatch>> {
    // Literal fuzzy words: punctuation does not enable hidden query operators.
    let pattern = Pattern::new(
      query,
      CaseMatching::Ignore,
      Normalization::Smart,
      AtomKind::Fuzzy,
    );
    jobs
      .iter()
      .map(|job| {
        let mut best = None;
        let mut indices: [Vec<u32>; 6] = std::array::from_fn(|_| Vec::new());
        for (field, text) in [
          &job.id,
          &job.name,
          &job.state,
          &job.queue,
          &job.owner,
          &job.reason,
        ]
        .iter()
        .enumerate()
        {
          let haystack = Utf32Str::new(text, &mut self.buffer);
          if let Some(score) = pattern.indices(haystack, &mut self.matcher, &mut indices[field]) {
            best = Some(best.map_or(score, |old: u32| old.max(score)));
            indices[field].sort_unstable();
            indices[field].dedup();
          } else {
            indices[field].clear();
          }
        }
        best.map(|score| JobMatch { score, indices })
      })
      .collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn job(name: &str) -> Job {
    Job {
      id: "123_4".into(),
      name: name.into(),
      owner: "alice".into(),
      state: "RUNNING".into(),
      queue: "gpu".into(),
      runtime: "10s".into(),
      nodes: "1".into(),
      cores: "8".into(),
      reason: "node01".into(),
    }
  }

  #[test]
  fn fuzzy_names_case_and_character_positions() {
    let result = Search::default().rank(&[job("climate-model")], "CLMOD");
    let matched = result[0].as_ref().unwrap();
    assert_eq!(matched.indices[1], vec![0, 1, 8, 9, 10]);
  }

  #[test]
  fn fields_are_independent_and_empty_query_matches_all() {
    let jobs = [job("simulation")];
    let mut search = Search::default();
    assert!(search.rank(&jobs, "123simulation")[0].is_none());
    for query in ["", "gp", "rnng", "1234", "alice"] {
      assert!(search.rank(&jobs, query)[0].is_some(), "{query}");
    }
    assert!(search.rank(&jobs, "zzzzzz")[0].is_none());
  }
}
