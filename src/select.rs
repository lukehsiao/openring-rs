//! Selection of the Articles to render from every projected Source.

use std::num::NonZeroUsize;

use jiff::Timestamp;
use rand::Rng;
use tracing::debug;

use crate::{
    Article,
    source::{Source, article_order},
};

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
/// feed actually has contribute nothing (see [`draw_weighted_slots`]). The
/// cutoff drops anything published at or after it before the caps apply,
/// and `num_articles` caps the final newest-first list.
///
/// Output is a function of the arguments alone: Sources are processed in
/// feed URL order, so a fixed `rng` reproduces the same picks no matter what
/// order the fetches finished in.
pub(crate) fn select(
    mut sources: Vec<Source>,
    policy: &Policy,
    rng: &mut impl Rng,
) -> Vec<Article> {
    // Fetches finish in nondeterministic order, and every weighted feed
    // consumes RNG draws, so a fixed iteration order is what makes a fixed
    // seed actually reproduce the output.
    sources.sort_unstable_by(|a, b| a.feed.url.cmp(&b.feed.url));

    let mut articles = Vec::new();
    for source in sources {
        let mut from_feed = source.articles;
        if let Some(cutoff) = policy.cutoff {
            from_feed.retain(|a| a.timestamp < cutoff);
        }
        match source.feed.weight {
            None => from_feed.truncate(policy.per_source),
            Some(weight) => {
                let eligible = from_feed.len();
                apply_weight(&mut from_feed, weight, policy.per_source, rng);
                if eligible > 0 && from_feed.is_empty() {
                    debug!(
                        source = source.feed.url.as_str(),
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

/// The 0-based ranks a feed with this weight fills on one run:
/// min(`per_source`, `weight`) distinct ranks drawn uniformly from
/// 0..weight, where rank i means "the feed's i-th newest eligible article".
///
/// Ranks come from the full 0..weight range on purpose. A rank the feed has
/// no article for is an empty slot, so a feed with E < N eligible articles
/// participates in roughly E/N of runs instead of concentrating its odds on
/// the few articles it has.
fn draw_weighted_slots(weight: NonZeroUsize, per_source: usize, rng: &mut impl Rng) -> Vec<usize> {
    rand::seq::index::sample(rng, weight.get(), per_source.min(weight.get())).into_vec()
}

/// Reduce a weighted feed's newest-first eligible articles to the drawn slots.
fn apply_weight(
    from_feed: &mut Vec<Article>,
    weight: NonZeroUsize,
    per_source: usize,
    rng: &mut impl Rng,
) {
    let slots = draw_weighted_slots(weight, per_source, rng);
    let mut rank = 0;
    from_feed.retain(|_| {
        let keep = slots.contains(&rank);
        rank += 1;
        keep
    });
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use hegel::{extras::rand as rand_gs, generators};
    use jiff::Timestamp;
    use rand::{SeedableRng, rngs::StdRng};
    use url::Url;

    use super::{Policy, draw_weighted_slots, select};
    use crate::{
        Article,
        feed_list::Feed,
        source::{Source, article_order},
        test_logs::warnings_logged,
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
        Article {
            link: url.join(link).expect("fixture link is valid"),
            title: title.to_owned(),
            summary: String::new(),
            source_link: url.clone(),
            source_title: url.host_str().unwrap_or_default().to_owned(),
            timestamp,
        }
    }

    // A Source at `url` holding `articles`, sorted the way projection
    // leaves them.
    fn source(url: &str, weight: Option<usize>, mut articles: Vec<Article>) -> Source {
        articles.sort_unstable_by(article_order);
        Source {
            feed: Feed {
                url: Url::parse(url).unwrap(),
                weight: weight.map(|w| NonZeroUsize::new(w).expect("weight is non-zero")),
            },
            articles,
        }
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
        articles.iter().map(|a| a.title.clone()).collect()
    }

    fn links(articles: &[Article]) -> Vec<String> {
        articles.iter().map(|a| a.link.to_string()).collect()
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
        let articles = select(vec![three_years()], &policy(1, 10), &mut rng(0));
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
        let articles = select(vec![old_and_new(None)], &policy, &mut rng(0));
        assert_eq!(titles(&articles), ["Old"]);
    }

    #[test]
    fn select_sorts_newest_first_and_caps_total() {
        // num_articles = 2 keeps the two newest, sorted newest-first.
        let articles = select(vec![three_years()], &policy(10, 2), &mut rng(0));
        assert_eq!(titles(&articles), ["Newest", "Middle"]);
        assert!(articles[0].timestamp > articles[1].timestamp);
    }

    #[test]
    fn select_orders_ties_deterministically() {
        // Sources arrive in fetch-completion order, which varies run to run.
        // Articles with identical timestamps must still come out in the same
        // order, or downstream static sites churn on every regeneration.
        let when: Timestamp = "2022-01-01T00:00:00Z".parse().unwrap();
        let make = |host: &str, title: &str| {
            let url = format!("https://{host}/feed.xml");
            let base = Url::parse(&url).unwrap();
            source(&url, None, vec![article(&base, title, "/post", when)])
        };
        let one = select(
            vec![make("a.example", "A"), make("b.example", "B")],
            &policy(10, 10),
            &mut rng(0),
        );
        let two = select(
            vec![make("b.example", "B"), make("a.example", "A")],
            &policy(10, 10),
            &mut rng(0),
        );
        assert_eq!(titles(&one), titles(&two));
    }

    #[test]
    fn select_drops_article_published_exactly_at_the_cutoff() {
        // An article published at the very instant the cutoff begins is not
        // "before" it and must be excluded.
        let cutoff = cutoff_2022();
        let url = Url::parse(FEED_URL).unwrap();
        let sources = vec![source(
            FEED_URL,
            None,
            vec![article(&url, "At Cutoff", "/at", cutoff)],
        )];
        let policy = Policy {
            cutoff: Some(cutoff),
            ..policy(10, 10)
        };
        let articles = select(sources, &policy, &mut rng(0));
        assert!(
            articles.is_empty(),
            "boundary article was kept: {articles:?}"
        );
    }

    #[test]
    fn select_drops_articles_after_the_cutoff() {
        let policy = Policy {
            cutoff: Some(cutoff_2022()),
            ..policy(10, 10)
        };
        let articles = select(vec![old_and_new(None)], &policy, &mut rng(0));
        assert_eq!(titles(&articles), ["Old"]);
    }

    #[test]
    fn select_is_quiet_when_only_the_cutoff_filters() {
        let policy = Policy {
            cutoff: Some("2000-01-01T00:00:00Z".parse().unwrap()),
            ..policy(10, 10)
        };
        let logs = warnings_logged(|| {
            assert!(select(vec![old_and_new(None)], &policy, &mut rng(0)).is_empty());
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
        let mut rng = tc.draw(rand_gs::randoms());

        let articles = select(
            vec![ranked_source(Some(weight), k)],
            &policy(per_source, usize::MAX),
            &mut rng,
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

    // The slot lottery itself: distinct ranks, inside the pool, exactly
    // min(per_source, weight) of them.
    #[hegel::test]
    fn draw_weighted_slots_draws_distinct_ranks_within_the_pool(tc: hegel::TestCase) {
        let weight = NonZeroUsize::new(tc.draw(generators::integers::<usize>().min_value(1)))
            .expect("min_value is 1");
        let per_source = tc.draw(generators::integers::<usize>().min_value(0).max_value(20));
        let mut rng = tc.draw(rand_gs::randoms());

        let mut slots = draw_weighted_slots(weight, per_source, &mut rng);

        assert_eq!(slots.len(), per_source.min(weight.get()));
        assert!(slots.iter().all(|&s| s < weight.get()));
        slots.sort_unstable();
        let drawn = slots.len();
        slots.dedup();
        assert_eq!(slots.len(), drawn, "slots must be distinct");
    }

    // Weight 1 pins the pool to the single newest article, which is exactly
    // what the unweighted path picks at per_source = 1.
    #[hegel::test]
    fn weight_one_matches_unweighted_selection(tc: hegel::TestCase) {
        let k = tc.draw(generators::integers::<usize>().min_value(0).max_value(10));
        let mut rng = tc.draw(rand_gs::randoms());

        let weighted = select(vec![ranked_source(Some(1), k)], &policy(1, 10), &mut rng);
        let unweighted = select(vec![ranked_source(None, k)], &policy(1, 10), &mut rng);

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

        let one = select(vec![ranked_source(None, k)], &policy, &mut rng(seed_a));
        let two = select(vec![ranked_source(None, k)], &policy, &mut rng(seed_b));

        assert_eq!(titles(&one), titles(&two));
    }

    // The --seed contract: a fixed seed reproduces the picks even though
    // fetches complete in arbitrary order. Fails without the feed URL sort
    // in select.
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

        let one = select(build(), &policy(1, 10), &mut rng(seed));
        let two = select(backward, &policy(1, 10), &mut rng(seed));

        assert_eq!(links(&one), links(&two));
    }

    #[hegel::test]
    fn select_output_never_exceeds_num_articles(tc: hegel::TestCase) {
        let sources = draw_sources(&tc);
        let policy = draw_policy(&tc);
        let mut rng = tc.draw(rand_gs::randoms());

        let articles = select(sources, &policy, &mut rng);
        assert!(articles.len() <= policy.num_articles);
    }

    // No Source exceeds its cap: per_source when unweighted, and
    // min(per_source, weight) when weighted.
    #[hegel::test]
    fn select_respects_each_sources_cap(tc: hegel::TestCase) {
        let sources = draw_sources(&tc);
        let policy = draw_policy(&tc);
        let mut rng = tc.draw(rand_gs::randoms());
        let caps: Vec<(Url, usize)> = sources
            .iter()
            .map(|s| {
                let cap = s
                    .feed
                    .weight
                    .map_or(policy.per_source, |w| policy.per_source.min(w.get()));
                (s.feed.url.clone(), cap)
            })
            .collect();

        let articles = select(sources, &policy, &mut rng);
        for (link, cap) in caps {
            let count = articles.iter().filter(|a| a.source_link == link).count();
            assert!(count <= cap, "{link} contributed {count}, cap {cap}");
        }
    }

    #[hegel::test]
    fn select_output_is_newest_first_with_ties_by_link(tc: hegel::TestCase) {
        let sources = draw_sources(&tc);
        let policy = draw_policy(&tc);
        let mut rng = tc.draw(rand_gs::randoms());

        let articles = select(sources, &policy, &mut rng);
        assert!(articles.windows(2).all(|w| {
            w[0].timestamp > w[1].timestamp
                || (w[0].timestamp == w[1].timestamp && w[0].link <= w[1].link)
        }));
    }

    #[hegel::test]
    fn select_keeps_nothing_at_or_after_the_cutoff(tc: hegel::TestCase) {
        let sources = draw_sources(&tc);
        let policy = draw_policy(&tc);
        let mut rng = tc.draw(rand_gs::randoms());

        let articles = select(sources, &policy, &mut rng);
        if let Some(cutoff) = policy.cutoff {
            assert!(articles.iter().all(|a| a.timestamp < cutoff));
        }
    }

    #[test]
    fn sparse_weighted_feed_sits_out_some_runs_without_warning() {
        let mut contributed = 0;
        let mut sat_out_seed = None;
        for seed in 0..200 {
            let articles = select(
                vec![ranked_source(Some(5), 1)],
                &policy(1, 10),
                &mut rng(seed),
            );
            match articles.as_slice() {
                [] => sat_out_seed = Some(seed),
                [only] => {
                    assert_eq!(only.title, "0", "the only eligible article is rank 0");
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
                &mut rng(sat_out_seed),
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
            let articles = select(
                vec![ranked_source(Some(2), 5)],
                &policy(3, 10),
                &mut rng(seed),
            );
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
            let articles = select(vec![old_and_new(Some(1))], &policy, &mut rng(seed));
            assert_eq!(titles(&articles), ["Old"]);
        }
    }
}
