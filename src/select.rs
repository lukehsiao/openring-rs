//! Selection of the Articles to render from every projected Source.

use std::num::NonZeroUsize;

use jiff::Timestamp;
use rand::{Rng, RngExt, SeedableRng, rngs::StdRng};
use tracing::debug;
use url::Url;

use crate::source::{Article, Source, article_order};

/// The user's limits on what renders.
#[derive(Debug)]
pub(crate) struct Policy {
    /// The most Articles one Source may contribute.
    pub(crate) per_source: usize,
    /// The most Articles in the output.
    pub(crate) num_articles: usize,
    /// Only Articles strictly before this instant qualify.
    pub(crate) cutoff: Option<Timestamp>,
}

/// Build the sorted, truncated list of articles to render from the Sources.
///
/// Each Source contributes its `per_source` most recent qualifying articles.
/// A Source whose Feed has weight N instead fills min(`per_source`, N) slots
/// drawn at random from its N most recent qualifying articles, so a prolific
/// feed stops monopolizing the output; slots drawn past the end of what the
/// feed actually has contribute nothing (see [`draw_weighted_ranks`]). The
/// cutoff drops anything published at or after it before the caps apply,
/// and `num_articles` caps the final newest-first list.
///
/// Output is a function of the arguments alone. Each weighted Feed draws
/// from its own random stream, seeded by `seed` and its URL, so one Feed's
/// picks do not depend on which other Feeds fetched or in what order.
pub(crate) fn select(mut sources: Vec<Source>, policy: &Policy, seed: u64) -> Vec<Article> {
    // Articles from different Sources can tie on date and link, so a fixed
    // Source order keeps the output the same whatever order the caller
    // passes the Sources in.
    sources.sort_unstable_by(|a, b| a.feed().url.cmp(&b.feed().url));
    let mut articles = Vec::new();
    for source in sources {
        let (feed, mut from_feed) = source.into_parts();
        if let Some(cutoff) = policy.cutoff {
            from_feed.retain(|a| a.timestamp() < cutoff);
        }
        match feed.weight {
            None => from_feed.truncate(policy.per_source),
            Some(weight) => {
                let eligible = from_feed.len();
                let mut rng = feed_rng(seed, &feed.url);
                let ranks = draw_weighted_ranks(weight, policy.per_source, eligible, &mut rng);
                // The ranks come out ascending, so one pass pairs them with
                // the newest-first articles.
                let mut ranks = ranks.into_iter().peekable();
                let mut rank = 0;
                from_feed.retain(|_| {
                    let keep = ranks.next_if_eq(&rank).is_some();
                    rank += 1;
                    keep
                });
                if eligible > 0 && from_feed.is_empty() {
                    debug!(
                        source = feed.url.as_str(),
                        weight = weight.get(),
                        eligible,
                        "weighted feed drew only empty slots and sits out this run"
                    );
                }
            }
        }
        articles.append(&mut from_feed);
    }

    articles.sort_unstable_by(article_order);
    articles.truncate(policy.num_articles);
    articles
}

/// The random stream for one Feed's draws, from the run's `seed` and the
/// Feed's URL. The mixing is FNV-1a, written out because std's hasher
/// may change between Rust releases. The draws themselves follow rand's
/// `StdRng`, so one seed can pick differently after a rand upgrade.
fn feed_rng(seed: u64, url: &Url) -> StdRng {
    // The 64-bit FNV offset basis and prime, from the FNV specification.
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mixed = seed
        .to_le_bytes()
        .iter()
        .chain(url.as_str().as_bytes())
        .fold(OFFSET, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(PRIME)
        });
    StdRng::seed_from_u64(mixed)
}

/// The 0-based ranks, below `eligible`, that a feed with this weight fills
/// on one run. Rank i means "the feed's i-th newest eligible article".
///
/// The draw is min(`per_source`, `weight`) distinct ranks, uniform over
/// 0..weight. Ranks come from the full 0..weight range on purpose. A rank
/// the feed has no article for is an empty slot, so a feed with E < N
/// eligible articles participates in roughly E/N of runs instead of
/// concentrating its odds on the few articles it has. Only ranks below
/// `eligible` can matter, so only those are decided, by selection sampling
/// (Knuth's Algorithm S). Time and memory are bounded by the articles the
/// feed has, not by the weight or `per_source`.
fn draw_weighted_ranks(
    weight: NonZeroUsize,
    per_source: usize,
    eligible: usize,
    rng: &mut impl Rng,
) -> Vec<usize> {
    let pool = weight.get();
    let mut wanted = per_source.min(pool);
    let mut ranks = Vec::new();
    for rank in 0..eligible.min(pool) {
        if wanted == 0 {
            break;
        }
        // Rank `rank` joins with probability wanted / (ranks left).
        if rng.random_range(0..pool - rank) < wanted {
            ranks.push(rank);
            wanted -= 1;
        }
    }
    ranks
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use hegel::generators;
    use jiff::Timestamp;
    use rand::{SeedableRng, rngs::StdRng};
    use url::Url;

    use super::{Policy, draw_weighted_ranks, select};
    use crate::{
        feed_list::Feed,
        source::{Article, Source},
        test_support::warnings_logged,
    };

    const FEED_URL: &str = "https://example.com/feed.xml";

    // One day in seconds, the spacing between ranked articles.
    const DAY: i64 = 86_400;

    // An arbitrary instant in 2020 that anchors every fixture date.
    const BASE: i64 = 1_600_000_000;

    fn at(second: i64) -> Timestamp {
        Timestamp::from_second(second).expect("timestamp in range")
    }

    fn article(url: &Url, title: &str, link: &str, timestamp: Timestamp) -> Article {
        let link = url.join(link).expect("fixture link is valid");
        Article::fixture(url, title, link, timestamp)
    }

    fn source(url: &str, weight: Option<usize>, articles: Vec<Article>) -> Source {
        let feed = Feed {
            url: Url::parse(url).unwrap(),
            weight: weight.map(|w| NonZeroUsize::new(w).expect("weight is non-zero")),
        };
        Source::fixture(feed, articles)
    }

    // A Source at `url` with `k` articles, where article `rank` (0-based)
    // is the rank-th newest and is titled with its rank.
    fn ranked_source_at(url: &str, weight: Option<usize>, k: usize) -> Source {
        let base = Url::parse(url).unwrap();
        let articles = (0..k)
            .map(|rank| {
                let day = i64::try_from(k - rank).expect("k is small");
                article(
                    &base,
                    &rank.to_string(),
                    &format!("{rank}"),
                    at(BASE + DAY * day),
                )
            })
            .collect();
        source(url, weight, articles)
    }

    fn ranked_source(weight: Option<usize>, k: usize) -> Source {
        ranked_source_at(FEED_URL, weight, k)
    }

    // Three dated articles titled by age, listed oldest first.
    fn three_years() -> Source {
        let url = Url::parse(FEED_URL).unwrap();
        source(
            FEED_URL,
            None,
            vec![
                article(
                    &url,
                    "Oldest",
                    "/1",
                    "2020-01-01T00:00:00Z".parse().unwrap(),
                ),
                article(
                    &url,
                    "Middle",
                    "/2",
                    "2021-01-01T00:00:00Z".parse().unwrap(),
                ),
                article(
                    &url,
                    "Newest",
                    "/3",
                    "2022-01-01T00:00:00Z".parse().unwrap(),
                ),
            ],
        )
    }

    // An article from 2020 and one from 2024, around a 2022 cutoff.
    fn old_and_new(weight: Option<usize>) -> Source {
        let url = Url::parse(FEED_URL).unwrap();
        source(
            FEED_URL,
            weight,
            vec![
                article(&url, "New", "/new", "2024-01-01T00:00:00Z".parse().unwrap()),
                article(&url, "Old", "/old", "2020-01-01T00:00:00Z".parse().unwrap()),
            ],
        )
    }

    fn cutoff_2022() -> Timestamp {
        "2022-01-01T00:00:00Z".parse().unwrap()
    }

    fn policy(per_source: usize, num_articles: usize) -> Policy {
        Policy {
            per_source,
            num_articles,
            cutoff: None,
        }
    }

    fn rng(seed: u64) -> StdRng {
        StdRng::seed_from_u64(seed)
    }

    fn titles(articles: &[Article]) -> Vec<String> {
        articles.iter().map(|a| a.title().to_owned()).collect()
    }

    fn links(articles: &[Article]) -> Vec<String> {
        articles.iter().map(|a| a.link().to_string()).collect()
    }

    // Up to three Sources on distinct hosts, each optionally weighted, with
    // a few articles on a coarse grid of days so timestamps tie often.
    fn draw_sources(tc: &hegel::TestCase) -> Vec<Source> {
        let hosts = ["a.example", "b.example", "c.example"];
        let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(3));
        hosts[..count]
            .iter()
            .map(|host| {
                let url = format!("https://{host}/feed.xml");
                let base = Url::parse(&url).unwrap();
                let weight = tc.draw(generators::optional(
                    generators::integers::<usize>().min_value(1).max_value(5),
                ));
                let days = tc.draw(
                    generators::vecs(generators::integers::<i64>().min_value(0).max_value(5))
                        .max_size(8),
                );
                let articles = days
                    .iter()
                    .enumerate()
                    .map(|(i, day)| {
                        article(
                            &base,
                            &i.to_string(),
                            &format!("/{i}"),
                            at(BASE + DAY * day),
                        )
                    })
                    .collect();
                source(&url, weight, articles)
            })
            .collect()
    }

    fn draw_policy(tc: &hegel::TestCase) -> Policy {
        Policy {
            per_source: tc.draw(generators::integers::<usize>().min_value(0).max_value(5)),
            num_articles: tc.draw(generators::integers::<usize>().min_value(0).max_value(10)),
            cutoff: tc
                .draw(generators::optional(
                    generators::integers::<i64>().min_value(0).max_value(6),
                ))
                .map(|day| at(BASE + DAY * day)),
        }
    }

    #[test]
    fn select_caps_articles_per_source() {
        // per_source = 1 keeps only the most recent article.
        let articles = select(vec![three_years()], &policy(1, 10), 0);
        assert_eq!(titles(&articles), ["Newest"]);
    }

    #[test]
    fn select_per_source_counts_only_qualifying_articles() {
        // The newest article falls after the cutoff; it must not use up the
        // per-source budget and shadow the older article that qualifies.
        let policy = Policy {
            cutoff: Some(cutoff_2022()),
            ..policy(1, 10)
        };
        let articles = select(vec![old_and_new(None)], &policy, 0);
        assert_eq!(titles(&articles), ["Old"]);
    }

    #[test]
    fn select_sorts_newest_first_and_caps_total() {
        // num_articles = 2 keeps the two newest, sorted newest-first.
        let articles = select(vec![three_years()], &policy(10, 2), 0);
        assert_eq!(titles(&articles), ["Newest", "Middle"]);
        assert!(articles[0].timestamp() > articles[1].timestamp());
    }

    #[test]
    fn select_is_quiet_when_only_the_cutoff_filters() {
        let policy = Policy {
            cutoff: Some("2000-01-01T00:00:00Z".parse().unwrap()),
            ..policy(10, 10)
        };
        let logs = warnings_logged(|| {
            assert!(select(vec![old_and_new(None)], &policy, 0).is_empty());
        });
        // --before is the user's own setting; filtering on it is not a fault
        // of the feed and must not warn.
        assert_eq!(logs, "", "expected no warnings for cutoff-only filtering");
    }

    // A weighted feed only ever contributes articles among its N newest
    // eligible ones, and never more than min(per_source, N) of them.
    #[hegel::test]
    fn weighted_selection_stays_within_the_n_newest(tc: hegel::TestCase) {
        let k = tc.draw(generators::integers::<usize>().min_value(0).max_value(40));
        let per_source = tc.draw(generators::integers::<usize>().min_value(0).max_value(10));
        // Unbounded weight: a huge pool must neither panic nor hang.
        let weight = tc.draw(generators::integers::<usize>().min_value(1));
        let seed = tc.draw(generators::integers::<u64>());

        let articles = select(
            vec![ranked_source(Some(weight), k)],
            &policy(per_source, usize::MAX),
            seed,
        );

        assert!(articles.len() <= per_source.min(weight));
        let mut ranks: Vec<usize> = titles(&articles)
            .iter()
            .map(|t| t.parse().expect("titles are ranks"))
            .collect();
        ranks.sort_unstable();
        let drawn = ranks.len();
        ranks.dedup();
        assert_eq!(ranks.len(), drawn, "every pick is a distinct article");
        assert!(
            ranks.iter().all(|&r| r < weight),
            "picks beyond the N newest: {ranks:?}"
        );
    }

    // The rank lottery itself: distinct ranks below what the feed has, and
    // exactly min(per_source, weight) of them whenever the feed has a full
    // pool to draw from.
    #[hegel::test]
    fn draw_weighted_ranks_draws_distinct_ranks_within_the_pool(tc: hegel::TestCase) {
        let weight = NonZeroUsize::new(tc.draw(generators::integers::<usize>().min_value(1)))
            .expect("min_value is 1");
        let per_source = tc.draw(generators::integers::<usize>());
        let eligible = tc.draw(generators::integers::<usize>().max_value(64));
        let seed = tc.draw(generators::integers::<u64>());

        let ranks = draw_weighted_ranks(weight, per_source, eligible, &mut rng(seed));

        assert!(ranks.windows(2).all(|w| w[0] < w[1]), "{ranks:?}");
        assert!(ranks.iter().all(|&r| r < eligible.min(weight.get())));
        let wanted = per_source.min(weight.get());
        if eligible >= weight.get() {
            assert_eq!(ranks.len(), wanted);
        } else {
            assert!(ranks.len() <= wanted);
        }
    }

    // Every rank in the pool is equally likely: with weight N and
    // per_source k, each rank fills a slot in k/N of runs. 40,000 seeded
    // runs put one standard deviation near 0.0025, so the 0.02 margin is
    // eight of them, and the seeds are fixed, so the test is deterministic.
    #[test]
    fn every_rank_in_the_pool_is_equally_likely() {
        let weight = NonZeroUsize::new(4).unwrap();
        let (per_source, runs) = (2, 40_000);
        let mut hits = [0_u32; 4];
        for seed in 0..runs {
            for rank in draw_weighted_ranks(weight, per_source, 4, &mut rng(seed)) {
                hits[rank] += 1;
            }
        }
        for (rank, &count) in hits.iter().enumerate() {
            let share = f64::from(count) / f64::from(u32::try_from(runs).unwrap());
            assert!((share - 0.5).abs() < 0.02, "rank {rank}: {share}");
        }
    }

    // Cost follows the articles a feed has, not its weight, so an
    // "unlimited" per_source with a huge weight returns at once.
    #[test]
    fn a_huge_weight_and_per_source_cost_nothing() {
        let weight = NonZeroUsize::new(usize::MAX).unwrap();
        let ranks = draw_weighted_ranks(weight, usize::MAX, 3, &mut rng(0));
        assert!(ranks.len() <= 3);
    }

    // A weighted feed that draws every one of many articles keeps them all,
    // in time linear in the articles. 300,000 articles is about 4.5e10 rank
    // comparisons for a quadratic pass, which takes minutes, against well
    // under a second for a linear one, so the 10 second bound has a wide
    // margin either way on a loaded machine.
    #[test]
    fn a_feed_that_draws_every_rank_keeps_every_article_in_linear_time() {
        const ARTICLES: usize = 300_000;
        let source = ranked_source_at("https://m.example/feed.xml", Some(ARTICLES), ARTICLES);
        let policy = policy(ARTICLES, usize::MAX);

        let started = std::time::Instant::now();
        let articles = select(vec![source], &policy, 0);
        let took = started.elapsed();

        assert_eq!(articles.len(), ARTICLES);
        assert!(took < std::time::Duration::from_secs(10), "took {took:?}");
    }

    // One weighted feed's picks do not depend on any other feed: adding or
    // losing a feed, such as one that failed to fetch, leaves the rest of a
    // seeded build unchanged.
    #[hegel::test]
    fn a_weighted_feeds_picks_ignore_the_other_feeds(tc: hegel::TestCase) {
        let seed = tc.draw(generators::integers::<u64>());
        let others =
            tc.draw(generators::vecs(generators::integers::<usize>().max_value(6)).max_size(3));
        let ours = || ranked_source_at("https://m.example/feed.xml", Some(5), 8);
        let mut sources = vec![ours()];
        for (i, &k) in others.iter().enumerate() {
            // Hosts on both sides of ours in URL order.
            let host = ["a", "z", "b"][i];
            sources.push(ranked_source_at(
                &format!("https://{host}.example/feed.xml"),
                Some(3),
                k,
            ));
        }

        let ours_only = |articles: Vec<Article>| -> Vec<String> {
            articles
                .into_iter()
                .filter(|a| a.source_link().as_str() == "https://m.example/feed.xml")
                .map(|a| a.link().to_string())
                .collect()
        };
        let wide = policy(3, usize::MAX);
        assert_eq!(
            ours_only(select(sources, &wide, seed)),
            ours_only(select(vec![ours()], &wide, seed))
        );
    }

    // Weight 1 pins the pool to the single newest article, which is exactly
    // what the unweighted path picks at per_source = 1.
    #[hegel::test]
    fn weight_one_matches_unweighted_selection(tc: hegel::TestCase) {
        let k = tc.draw(generators::integers::<usize>().min_value(0).max_value(10));
        let seed = tc.draw(generators::integers::<u64>());

        let weighted = select(vec![ranked_source(Some(1), k)], &policy(1, 10), seed);
        let unweighted = select(vec![ranked_source(None, k)], &policy(1, 10), seed);

        assert_eq!(titles(&weighted), titles(&unweighted));
    }

    // Without weights the RNG must be inert: any two seeds give identical
    // output, so unweighted users keep deterministic builds.
    #[hegel::test]
    fn unweighted_selection_ignores_the_rng(tc: hegel::TestCase) {
        let k = tc.draw(generators::integers::<usize>().min_value(0).max_value(10));
        let per_source = tc.draw(generators::integers::<usize>().min_value(0).max_value(3));
        let num_articles = tc.draw(generators::integers::<usize>().min_value(0).max_value(5));
        let seed_a = tc.draw(generators::integers::<u64>());
        let seed_b = tc.draw(generators::integers::<u64>());
        let policy = policy(per_source, num_articles);

        let one = select(vec![ranked_source(None, k)], &policy, seed_a);
        let two = select(vec![ranked_source(None, k)], &policy, seed_b);

        assert_eq!(titles(&one), titles(&two));
    }

    // The --seed contract: a fixed seed reproduces the picks even though
    // fetches complete in arbitrary order.
    #[hegel::test]
    fn same_seed_reproduces_selection_regardless_of_fetch_order(tc: hegel::TestCase) {
        let seed = tc.draw(generators::integers::<u64>());
        let hosts = ["a.example", "b.example", "c.example"];
        let mut shape = Vec::new();
        for _ in hosts {
            let k = tc.draw(generators::integers::<usize>().min_value(0).max_value(8));
            let weight = tc.draw(generators::optional(
                generators::integers::<usize>().min_value(1).max_value(5),
            ));
            shape.push((k, weight));
        }
        let build = || -> Vec<Source> {
            hosts
                .iter()
                .zip(&shape)
                .map(|(host, &(k, weight))| {
                    ranked_source_at(&format!("https://{host}/feed.xml"), weight, k)
                })
                .collect()
        };
        let mut backward = build();
        backward.reverse();

        let one = select(build(), &policy(1, 10), seed);
        let two = select(backward, &policy(1, 10), seed);

        assert_eq!(links(&one), links(&two));
    }

    // Two Sources can list the same post at the same instant, which ties
    // under article order. The Source order in select keeps such ties in
    // the same order whatever order the Sources arrive in, so a static
    // site does not churn on every rebuild.
    #[test]
    fn ties_across_sources_render_the_same_whatever_the_fetch_order() {
        let both = |hosts: [&str; 2]| -> Vec<Source> {
            hosts
                .iter()
                .map(|host| {
                    let url = format!("https://{host}/feed.xml");
                    let base = Url::parse(&url).unwrap();
                    let post = article(&base, host, "https://shared.example/post", at(BASE));
                    source(&url, None, vec![post])
                })
                .collect()
        };
        let one = select(both(["a.example", "b.example"]), &policy(1, 10), 0);
        let two = select(both(["b.example", "a.example"]), &policy(1, 10), 0);
        assert_eq!(titles(&one), titles(&two));
    }

    // The Feed URL enters each stream, so two weighted Feeds of the same
    // shape do not draw the same ranks on every run.
    #[test]
    fn feeds_of_the_same_shape_draw_independently() {
        let picks = |seed: u64| -> (Vec<String>, Vec<String>) {
            let articles = select(
                vec![
                    ranked_source_at("https://a.example/feed.xml", Some(8), 8),
                    ranked_source_at("https://b.example/feed.xml", Some(8), 8),
                ],
                &policy(1, 10),
                seed,
            );
            let from = |host: &str| -> Vec<String> {
                articles
                    .iter()
                    .filter(|a| a.source_link().host_str() == Some(host))
                    .map(|a| a.title().to_owned())
                    .collect()
            };
            (from("a.example"), from("b.example"))
        };
        // With one pick from eight, two independent Feeds agree on 1 in 8
        // runs, so 20 seeds that all agree would mean the streams match.
        assert!(
            (0..20).any(|seed| {
                let (a, b) = picks(seed);
                a != b
            }),
            "every seed drew the same ranks for both feeds"
        );
    }

    #[hegel::test]
    fn select_output_never_exceeds_num_articles(tc: hegel::TestCase) {
        let sources = draw_sources(&tc);
        let policy = draw_policy(&tc);
        let seed = tc.draw(generators::integers::<u64>());

        let articles = select(sources, &policy, seed);
        assert!(articles.len() <= policy.num_articles);
    }

    // No Source exceeds its cap: per_source when unweighted, and
    // min(per_source, weight) when weighted.
    #[hegel::test]
    fn select_respects_each_sources_cap(tc: hegel::TestCase) {
        let sources = draw_sources(&tc);
        let policy = draw_policy(&tc);
        let seed = tc.draw(generators::integers::<u64>());
        let caps: Vec<(Url, usize)> = sources
            .iter()
            .map(|s| {
                let cap = s
                    .feed()
                    .weight
                    .map_or(policy.per_source, |w| policy.per_source.min(w.get()));
                (s.feed().url.clone(), cap)
            })
            .collect();

        let articles = select(sources, &policy, seed);
        for (link, cap) in caps {
            let count = articles.iter().filter(|a| *a.source_link() == link).count();
            assert!(count <= cap, "{link} contributed {count}, cap {cap}");
        }
    }

    #[hegel::test]
    fn select_output_is_newest_first_with_ties_by_link(tc: hegel::TestCase) {
        let sources = draw_sources(&tc);
        let policy = draw_policy(&tc);
        let seed = tc.draw(generators::integers::<u64>());

        let articles = select(sources, &policy, seed);
        assert!(articles.windows(2).all(|w| {
            w[0].timestamp() > w[1].timestamp()
                || (w[0].timestamp() == w[1].timestamp() && w[0].link() <= w[1].link())
        }));
    }

    #[hegel::test]
    fn select_keeps_nothing_at_or_after_the_cutoff(tc: hegel::TestCase) {
        let sources = draw_sources(&tc);
        let policy = draw_policy(&tc);
        let seed = tc.draw(generators::integers::<u64>());

        let articles = select(sources, &policy, seed);
        if let Some(cutoff) = policy.cutoff {
            assert!(articles.iter().all(|a| a.timestamp() < cutoff));
        }
    }

    #[test]
    fn sparse_weighted_feed_sits_out_some_runs_without_warning() {
        let mut contributed = 0;
        let mut sat_out_seed = None;
        for seed in 0..200 {
            let articles = select(vec![ranked_source(Some(5), 1)], &policy(1, 10), seed);
            match articles.as_slice() {
                [] => sat_out_seed = Some(seed),
                [only] => {
                    assert_eq!(only.title(), "0", "the only eligible article is rank 0");
                    contributed += 1;
                }
                more => panic!("one eligible article cannot yield {} picks", more.len()),
            }
        }
        // One article in a pool of five contributes on ~1/5 of seeds; both
        // outcomes are a statistical certainty across 200 seeds.
        assert!(contributed > 0, "feed never contributed");
        let sat_out_seed = sat_out_seed.expect("feed never sat out");

        let logs = warnings_logged(|| {
            let articles = select(
                vec![ranked_source(Some(5), 1)],
                &policy(1, 10),
                sat_out_seed,
            );
            assert!(articles.is_empty());
        });
        // Sitting out the lottery is the feature working, not a feed fault.
        assert_eq!(logs, "", "sitting out must not warn");
    }

    #[test]
    fn weight_caps_contribution_below_per_source() {
        // Weight 2 with per_source 3: both slots in the pool of two are
        // always drawn, so exactly the two newest come back on any seed.
        for seed in 0..20 {
            let articles = select(vec![ranked_source(Some(2), 5)], &policy(3, 10), seed);
            let mut got = titles(&articles);
            got.sort_unstable();
            assert_eq!(got, ["0", "1"]);
        }
    }

    #[test]
    fn weighted_slots_count_only_qualifying_articles() {
        // The newest article falls after the cutoff; with weight 1 the pick
        // must be the newest qualifying article, exactly like the unweighted
        // path.
        let policy = Policy {
            cutoff: Some(cutoff_2022()),
            ..policy(1, 10)
        };
        for seed in 0..20 {
            let articles = select(vec![old_and_new(Some(1))], &policy, seed);
            assert_eq!(titles(&articles), ["Old"]);
        }
    }
}
