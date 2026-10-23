# openring

openring fetches a configured set of web feeds and renders their most recent articles through a template, as a webring for static sites.

## Configuration

**Feed**:
A web feed the user configured by URL, with an optional Weight.
_Avoid_: URL, subscription, channel

**Weight**:
A number N on a Feed that makes it fill its slots at random from its N newest Articles, so a prolific Feed stops crowding out the rest.
_Avoid_: priority, frequency

**Feed list**:
Every configured Feed, merged from the command line and the urls file, with one Weight per Feed.
_Avoid_: urls, feed set

## Content

**Entry**:
One item as a fetched Feed's document lists it, complete or not.
_Avoid_: item, post

**Source**:
A fetched Feed as a reader meets it: its display title, its homepage link, and its Articles.
_Avoid_: site, blog, publisher

**Article**:
An Entry with a web link, a title a reader can see, and a date, tied to its Source, ready to render.
_Avoid_: post, story, link

## Fetching

**Validators**:
The ETag and Last-Modified values a server sent with a Feed, replayed so the server can answer that nothing changed.
_Avoid_: cache headers, conditional headers

**Retry window**:
The time after a server answers "too many requests" during which openring serves the cached Feed without asking again.
_Avoid_: backoff, rate limit, cooldown
