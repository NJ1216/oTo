use std::collections::HashMap;
use std::future::Future;
use tokio::task::{Id, JoinSet};

/// Retain input identity outside the future so a panic cannot discard it.
pub(super) struct IdentifiedTasks<Meta, Output> {
    tasks: JoinSet<Output>,
    identities: HashMap<Id, Meta>,
}

impl<Meta, Output: Send + 'static> IdentifiedTasks<Meta, Output> {
    pub fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
            identities: HashMap::new(),
        }
    }

    pub fn spawn(&mut self, metadata: Meta, task: impl Future<Output = Output> + Send + 'static) {
        let handle = self.tasks.spawn(task);
        self.identities.insert(handle.id(), metadata);
    }

    pub async fn join_next(&mut self) -> Option<(Meta, Result<Output, String>)> {
        let (id, result) = match self.tasks.join_next_with_id().await? {
            Ok((id, result)) => (id, Ok(result)),
            Err(error) => (
                error.id(),
                Err(format!("input preparation task failed: {error}")),
            ),
        };
        Some((
            self.identities.remove(&id).expect("task identity exists"),
            result,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn panic_retains_every_group_member_and_other_groups_complete() {
        let mut tasks = IdentifiedTasks::new();
        tasks.spawn(vec!["a.wav", "a.flac"], async {
            panic!("injected probe failure");
        });
        tasks.spawn(vec!["b.wav"], async { 42 });
        let mut completed = Vec::new();
        while let Some(result) = tasks.join_next().await {
            completed.push(result);
        }
        completed.sort_by_key(|(paths, _)| paths[0]);
        assert_eq!(completed[0].0, ["a.wav", "a.flac"]);
        assert!(completed[0]
            .1
            .as_ref()
            .unwrap_err()
            .contains("injected probe failure"));
        assert_eq!(completed[1], (vec!["b.wav"], Ok(42)));
        assert!(tasks.identities.is_empty());
    }
}
