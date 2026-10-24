//! The fan-out module: run one task per URL with bounded concurrency, show
//! progress, and report each task's outcome exactly once. Feed fetches and
//! summary-page fetches both go through it, so the cap, the progress bar,
//! the pending list, and the rule for where a problem is reported live in
//! one place.

use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use tokio::{sync::Semaphore, task::JoinSet};
use tracing::warn;
use url::Url;

use crate::terminal::{Bar, Terminal, Tone};

/// Cap on in-flight tasks so a long urls file cannot exhaust the process's
/// file descriptors (macOS defaults to 256 per process). 32 keeps the network
/// saturated while staying well below that floor.
const MAX_IN_FLIGHT: usize = 32;

/// How one task ended.
#[derive(Debug)]
pub(crate) enum Outcome<T> {
    /// It produced a value.
    Done(T),
    /// It produced a value, but from a fallback, for the reason given.
    Stale(T, String),
    /// It produced nothing, and that is routine, so nothing is reported.
    Skipped,
    /// It failed for the reason given.
    Failed(String),
}

/// The words one fan-out uses on the bar and in the log. Every caller gives
/// all of them, even words its tasks never trigger. One shape for both
/// callers keeps the reporting rule in one place, and a struct per caller
/// would need the same fields.
#[derive(Debug)]
pub(crate) struct Words {
    /// The bar's label, such as "Fetching".
    pub(crate) progress: &'static str,
    /// The status word for a task that is done, such as "Fetched".
    pub(crate) done: &'static str,
    /// The log message for a stale result when nobody watches the bar.
    pub(crate) stale_log: &'static str,
    /// The log message for a failure when nobody watches the bar.
    pub(crate) failed_log: &'static str,
    /// The log message for a task that panicked.
    pub(crate) panic_log: &'static str,
}

/// Run `task` once for each item, at most [`MAX_IN_FLIGHT`] at a time, and
/// return the values of the tasks that were done or stale, in item order.
///
/// Each item comes with the URL it concerns, which the bar shows while the
/// task runs and the report names when it ends. A done task prints a status
/// line. A stale or failed task prints a status line when someone can see
/// the bar, and logs a warning otherwise, because a hidden bar loses its
/// lines. A task that panics loses only its own value, and is both logged
/// and shown.
pub(crate) async fn fan_out<I, T, F, Fut>(
    terminal: &Terminal,
    words: &Words,
    items: Vec<(Url, I)>,
    task: F,
) -> Vec<T>
where
    T: Send + 'static,
    F: Fn(Url, I) -> Fut,
    Fut: Future<Output = Outcome<T>> + Send + 'static,
{
    if items.is_empty() {
        return Vec::new();
    }
    let bar = terminal.bar(words.progress, items.len());
    let semaphore = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    let mut join_set = JoinSet::new();
    let mut urls = Vec::with_capacity(items.len());
    // Which item each task works on, so a task that panics, and so returns
    // no index of its own, can still be named.
    let mut index_of = HashMap::with_capacity(items.len());
    for (index, (url, item)) in items.into_iter().enumerate() {
        let work = task(url.clone(), item);
        let semaphore = Arc::clone(&semaphore);
        let handle = join_set.spawn(async move {
            // acquire_owned errors only when the semaphore is closed, and
            // this one never is.
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("semaphore is never closed");
            (index, work.await)
        });
        index_of.insert(handle.id(), index);
        urls.push(url);
    }

    let mut pending: BTreeSet<usize> = (0..urls.len()).collect();
    show_pending(&bar, &urls, &pending);
    let mut values: Vec<Option<T>> = std::iter::repeat_with(|| None).take(urls.len()).collect();
    while let Some(joined) = join_set.join_next().await {
        bar.inc();
        let (index, joined) = match joined {
            Ok((index, outcome)) => (index, Ok(outcome)),
            Err(e) => (index_of[&e.id()], Err(e)),
        };
        pending.remove(&index);
        show_pending(&bar, &urls, &pending);
        let url = &urls[index];
        match joined {
            // A task that panicked loses its value, but the failure must not
            // be silent, even when someone watches the bar.
            Err(e) => {
                warn!(source = url.as_str(), error = %e, "{}", words.panic_log);
                bar.status("Error", Tone::Bad, format!("{url} ({e})"));
            }
            Ok(Outcome::Done(value)) => {
                bar.status(words.done, Tone::Good, url);
                values[index] = Some(value);
            }
            Ok(Outcome::Stale(value, cause)) => {
                if terminal.is_visible() {
                    bar.status("Stale", Tone::Warn, format!("{url} ({cause})"));
                } else {
                    warn!(source = url.as_str(), error = %cause, "{}", words.stale_log);
                }
                values[index] = Some(value);
            }
            Ok(Outcome::Skipped) => {}
            Ok(Outcome::Failed(cause)) => {
                if terminal.is_visible() {
                    bar.status("Error", Tone::Bad, format!("{url} ({cause})"));
                } else {
                    warn!(source = url.as_str(), error = %cause, "{}", words.failed_log);
                }
            }
        }
    }
    bar.finish_and_clear();
    values.into_iter().flatten().collect()
}

/// Show the URLs still in flight beside the bar.
fn show_pending(bar: &Bar, urls: &[Url], pending: &BTreeSet<usize>) {
    bar.set_message(
        pending
            .iter()
            .map(|&i| urls[i].as_str())
            .collect::<Vec<_>>()
            .join(", "),
    );
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use hegel::generators;
    use url::Url;

    use super::{MAX_IN_FLIGHT, Outcome, Words, fan_out};
    use crate::{
        terminal::{Line, Terminal, Tone},
        test_support::{block_on, warnings_logged},
    };

    const WORDS: Words = Words {
        progress: "Testing",
        done: "Tested",
        stale_log: "used a fallback",
        failed_log: "task failed",
        panic_log: "task panicked",
    };

    // The outcome a test asks one task to end with.
    #[derive(Debug, Clone, Copy)]
    enum Ends {
        Done,
        Stale,
        Skipped,
        Failed,
    }

    fn url(i: usize) -> Url {
        Url::parse(&format!("https://item{i}.example/")).unwrap()
    }

    #[hegel::composite]
    fn endings(tc: &hegel::TestCase) -> Vec<Ends> {
        tc.draw(
            generators::vecs(generators::sampled_from(vec![
                Ends::Done,
                Ends::Stale,
                Ends::Skipped,
                Ends::Failed,
            ]))
            .max_size(40),
        )
    }

    // Run one task per ending, each ending as told, and return the values,
    // the bar's lines, and the log.
    fn run(visible: bool, ends: &[Ends]) -> (Vec<usize>, Vec<Line>, String) {
        let terminal = Terminal::create_null(visible);
        let lines = terminal.track_lines();
        let items = ends
            .iter()
            .enumerate()
            .map(|(i, &e)| (url(i), (i, e)))
            .collect();
        let mut values = Vec::new();
        let logs = warnings_logged(|| {
            values = block_on(fan_out(&terminal, &WORDS, items, |_, (i, e)| async move {
                match e {
                    Ends::Done => Outcome::Done(i),
                    Ends::Stale => Outcome::Stale(i, format!("cause {i}")),
                    Ends::Skipped => Outcome::Skipped,
                    Ends::Failed => Outcome::Failed(format!("cause {i}")),
                }
            }));
        });
        (values, lines.data(), logs)
    }

    // Done and stale tasks return their values in item order; skipped and
    // failed tasks return nothing.
    #[hegel::test]
    fn values_come_back_in_item_order(tc: hegel::TestCase) {
        let ends = tc.draw(endings());
        let visible = tc.draw(generators::booleans());
        let (values, _, _) = run(visible, &ends);
        let expected: Vec<usize> = ends
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, Ends::Done | Ends::Stale))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(values, expected);
    }

    // When someone watches, every done, stale, and failed task shows
    // exactly one line, a skipped task shows none, and nothing is logged.
    #[hegel::test]
    fn a_visible_bar_reports_each_task_once_on_the_bar(tc: hegel::TestCase) {
        let ends = tc.draw(endings());
        let (_, mut lines, logs) = run(true, &ends);
        let mut expected: Vec<Line> = ends
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let (word, tone, detail) = match e {
                    Ends::Done => ("Tested", Tone::Good, url(i).to_string()),
                    Ends::Stale => ("Stale", Tone::Warn, format!("{} (cause {i})", url(i))),
                    Ends::Failed => ("Error", Tone::Bad, format!("{} (cause {i})", url(i))),
                    Ends::Skipped => return None,
                };
                Some(Line { word, tone, detail })
            })
            .collect();
        // Tasks finish in any order; each line must appear exactly once.
        let key = |l: &Line| l.detail.clone();
        lines.sort_by_key(key);
        expected.sort_by_key(key);
        assert_eq!(lines, expected);
        assert_eq!(logs, "");
    }

    // When nobody watches, each stale and failed task logs exactly one
    // warning naming its URL, and only done tasks reach the bar.
    #[hegel::test]
    fn a_hidden_bar_reports_problems_once_in_the_log(tc: hegel::TestCase) {
        let ends = tc.draw(endings());
        let (_, lines, logs) = run(false, &ends);
        for (i, e) in ends.iter().enumerate() {
            let source = format!("source=\"{}\"", url(i));
            let logged = logs.lines().filter(|l| l.contains(&source)).count();
            let expected = usize::from(matches!(e, Ends::Stale | Ends::Failed));
            assert_eq!(logged, expected, "item {i} ({e:?}): {logs}");
        }
        assert!(
            lines.iter().all(|l| l.word == "Tested"),
            "a hidden bar showed a problem line: {lines:?}"
        );
    }

    // However many items there are, no more than the cap run at once, and
    // every item still runs.
    #[hegel::test]
    fn in_flight_tasks_never_exceed_the_cap(tc: hegel::TestCase) {
        let n = tc.draw(generators::integers::<usize>().max_value(3 * MAX_IN_FLIGHT));
        // How often each task yields while it holds its slot, so tasks
        // overlap in different ways.
        let yields = tc.draw(generators::integers::<usize>().max_value(4));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let items = (0..n).map(|i| (url(i), ())).collect();

        let values = block_on(fan_out(
            &Terminal::create_null(false),
            &WORDS,
            items,
            |_, ()| {
                let (in_flight, most) = (Arc::clone(&in_flight), Arc::clone(&most));
                async move {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    for _ in 0..yields {
                        tokio::task::yield_now().await;
                    }
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Outcome::Done(())
                }
            },
        ));

        assert_eq!(values.len(), n);
        assert!(most.load(Ordering::SeqCst) <= MAX_IN_FLIGHT);
    }

    #[test]
    fn a_panicking_task_loses_only_its_own_value() {
        let terminal = Terminal::create_null(true);
        let lines = terminal.track_lines();
        let items = (0..3).map(|i| (url(i), i)).collect();
        let mut values = Vec::new();
        let logs = warnings_logged(|| {
            values = block_on(fan_out(&terminal, &WORDS, items, |_, i| async move {
                assert_ne!(i, 1, "the task for item 1 panics on purpose");
                Outcome::Done(i)
            }));
        });

        assert_eq!(values, [0, 2]);
        // The panic is named by its URL in the log and on the bar.
        assert!(logs.contains("task panicked"), "{logs}");
        assert!(logs.contains(&format!("source=\"{}\"", url(1))), "{logs}");
        let errors: Vec<Line> = lines
            .data()
            .into_iter()
            .filter(|l| l.word == "Error")
            .collect();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].detail.starts_with(url(1).as_str()), "{errors:?}");
    }
}
