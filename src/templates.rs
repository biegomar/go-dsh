//! Prompt templates for dsh workers and the workspace AGENTS.md. Workers run
//! unattended, so every template states the deliverable files explicitly.

use chrono::Utc;

pub const TEMPLATES: &[&str] = &["raw", "research", "materials"];
pub const DEPTHS: &[&str] = &["quick", "standard", "deep"];

/// Material kinds with their human description, in display order.
pub const MATERIAL_KINDS: &[(&str, &str)] = &[
    ("screenshots", "screenshots of blogs, websites and articles"),
    ("scroll", "scroll videos (screen recordings) of blog posts and websites"),
    ("articles", "news articles with a screenshot of the headline"),
    ("stock-video", "stock footage clips from free sources"),
    ("stock-photo", "stock photos from free sources"),
];

pub fn material_description(kind: &str) -> Option<&'static str> {
    MATERIAL_KINDS.iter().find(|(k, _)| *k == kind).map(|(_, d)| *d)
}

pub fn material_kind_names() -> String {
    MATERIAL_KINDS.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")
}

fn today() -> String {
    Utc::now().format("%Y-%m-%d").to_string()
}

const WORKER_PREAMBLE: &str = "You are an unattended dsh worker. Another agent started you through go-dsh and will only read your files and your final answer. Nobody is in the loop: do not ask questions, make sensible assumptions and write them down, look for alternatives instead of giving up. All files go into your working folder. Write in the language of the task.";

/// Header naming the job's output folder. Web batches share one Workspace
/// directory as session cwd, so each worker needs its own folder spelled out.
pub fn workdir_header(out_dir: &str) -> String {
    format!(
        "Working folder for this task: {out_dir}\nPut every file there and nowhere else. Run shell commands as `cd \"{out_dir}\" && ...` or use absolute paths below it. Other folders in the workspace belong to parallel jobs; do not touch them.\n\n"
    )
}

fn depth_rule(depth: &str) -> &'static str {
    match depth {
        "quick" => "Depth quick: 3 to 5 good sources, focus on what matters, finish fast.",
        "deep" => "Depth deep: 10 or more sources, prefer primary sources, name counter positions and uncertainty explicitly.",
        _ => "Depth standard: 6 to 10 sources, check numbers and key claims against at least two sources.",
    }
}

const RESEARCH_TEMPLATE: &str = r#"@@PREAMBLE@@

## Research question
@@QUESTION@@

## Method
- Today is @@TODAY@@. Prefer current information and note the date of every source.
- Use the best search and fetch tools you have (for example firecrawl or Brave Search MCP tools if present, context7 for library docs); built-in web_search and web_fetch are the fallback.
- @@DEPTH@@
- Never invent content. Mark anything you could not verify as open.
@@EXTRA@@
## Required output (files in your working folder)
1. `report.md`: title, summary (3 to 5 sentences), key facts as a list with source references like [1], details, open questions, numbered source list with URL, title, publisher, date.
2. `sources.json`: JSON array of objects {"url", "title", "publisher", "date", "relevance" (1 to 5), "notes"}.

## Final answer
Only the summary and the 3 most important facts, at most 150 words. The full report lives in report.md."#;

pub fn research_prompt(question: &str, depth: &str, extra: Option<&str>) -> String {
    let extra = extra.map(|e| format!("- Additional instruction: {}\n", e.trim())).unwrap_or_default();
    RESEARCH_TEMPLATE
        .replace("@@PREAMBLE@@", WORKER_PREAMBLE)
        .replace("@@TODAY@@", &today())
        .replace("@@DEPTH@@", depth_rule(depth))
        .replace("@@EXTRA@@", &extra)
        .replace("@@QUESTION@@", question.trim())
}

fn kind_rules(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "screenshots" => "Find relevant blog posts, articles and websites on the topic and take screenshots.\n- Tool: `go-dsh capture shot <url> -o screenshots/<nn>-<short-name>.png` (1440x900 viewport, cookie banners and overlays are removed automatically). Whole page: `--full`; one element: `--selector \"<css>\"`.\n- Check every screenshot (open or view it): no cookie banner, no paywall overlay, readable content. Otherwise capture again or replace it.",
        "scroll" => "Create scroll videos (smooth scrolling) of relevant blog posts and websites.\n- Tool: `go-dsh capture scroll <url> -o scroll/<nn>-<short-name>.mp4 --duration 12` (1920x1080, H.264, cookie banners removed automatically).\n- Check every video with ffprobe (duration, resolution) and one still (`ffmpeg -ss 3 -i file.mp4 -frames:v 1 check.png`). Record again when a banner or a blank frame shows.",
        "articles" => "Find current news articles on the topic, local and international.\n- For each article: a headline screenshot with `go-dsh capture shot <url> -o articles/<nn>-<outlet>.png` (viewport, not the full page) and an excerpt in articles/<nn>-<outlet>.md (headline, outlet, date, author, 3 to 5 key points, URL).\n- Paywalled articles only when headline and teaser are visible.",
        "stock-video" => "Find matching stock footage clips from free sources (Pexels, Pixabay, Coverr, Mixkit, Videvo with a free licence).\n- Download them as MP4, preferably 1920x1080 or 4K, with curl from the direct file URL or with yt-dlp. Save them as stock-video/<nn>-<short-name>.mp4.\n- Check every file with ffprobe (duration, resolution, codec). Record the licence exactly.",
        "stock-photo" => "Find matching stock photos from free sources (Unsplash, Pexels, Pixabay).\n- Download them in high resolution as stock-photo/<nn>-<short-name>.jpg.\n- Record photographer, source and licence exactly.",
        _ => return None,
    })
}

const MATERIALS_TEMPLATE: &str = r#"@@PREAMBLE@@

## Task: gather material (@@DESC@@)
Topic and brief:
@@BRIEF@@

## Method
- Today is @@TODAY@@.
- Goal: about @@COUNT@@ high quality results that fit the topic. Quality over quantity.
- Find sources with your search tools (web, news, images, videos).
@@URLS@@- @@RULES@@
- File names: two-digit running number plus a short descriptive name, lowercase letters, digits and hyphens only.

## Required output (files in your working folder)
1. The material files in the subfolder `@@KIND@@/`.
2. `manifest.json`: JSON array, one object per file: {"file" (relative path), "kind": "@@KIND@@", "source_url", "title", "publisher", "author", "license", "date", "notes"}.
3. `README.md`: a short overview table (file, content, source, licence).

## Final answer
Number of files, one line per file (file name: content), problems if any. At most 200 words."#;

pub fn materials_prompt(brief: &str, kind: &str, urls: &[String], count: u32) -> Option<String> {
    let rules = kind_rules(kind)?;
    let desc = material_description(kind)?;
    let urls_block = if urls.is_empty() {
        String::new()
    } else {
        format!("- Process these URLs first, they are mandatory:\n{}\n", urls.iter().map(|u| format!("  - {u}")).collect::<Vec<_>>().join("\n"))
    };
    Some(
        MATERIALS_TEMPLATE
            .replace("@@PREAMBLE@@", WORKER_PREAMBLE)
            .replace("@@DESC@@", desc)
            .replace("@@TODAY@@", &today())
            .replace("@@COUNT@@", &count.to_string())
            .replace("@@URLS@@", &urls_block)
            .replace("@@RULES@@", rules)
            .replace("@@KIND@@", kind)
            .replace("@@BRIEF@@", brief.trim()),
    )
}

pub fn workspace_agents_md(name: &str, kind: &str, description: Option<&str>) -> String {
    let desc = description.map(|d| format!("{}\n\n", d.trim())).unwrap_or_default();
    format!(
        "# dsh workspace: {name}\n\n{desc}This folder is a workspace for dsh workers started through go-dsh.\nKind: {kind}. Created {today}.\n\n\
## Conventions for workers\n\
- Work only in your working folder (named at the top of your task) and below it. Leave other job folders alone.\n\
- Deliver results as files; the final answer is only a summary.\n\
- Research: `report.md` plus `sources.json`.\n\
- Material: files in the subfolder of the material kind plus `manifest.json` and `README.md`.\n\
- Take screenshots and scroll videos with `go-dsh capture shot` and `go-dsh capture scroll`.\n\
- Always record licences and sources exactly.\n\n\
## Layout\n\
- `<batch>/<nn-job>/`: one folder per job of a batch\n\
- `<batch>/INDEX.md`: overview, written by `go-dsh collect <batch>`\n",
        today = today()
    )
}
