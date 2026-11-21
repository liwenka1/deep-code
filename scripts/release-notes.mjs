import { readFileSync, writeFileSync } from "node:fs";

const changelogPath = "CHANGELOG.md";
const reviewedStart = "<!-- reviewed-notes:start -->";
const reviewedEnd = "<!-- reviewed-notes:end -->";
const reviewedBlock = new RegExp(`^${reviewedStart}\n[\\s\\S]*?\n${reviewedEnd}$`, "gm");

function readSections(changelog) {
  const headings = [...changelog.matchAll(/^## \[(Unreleased|\d+\.\d+\.\d+)\][^\n]*$/gm)];
  return headings.map((heading, index) => ({
    version: heading[1],
    start: heading.index,
    body: changelog.slice(heading.index + heading[0].length, headings[index + 1]?.index).trim(),
  }));
}

function reviewedNotes(body) {
  return Array.from(body.matchAll(reviewedBlock), ([note]) => note);
}

function prepareChangelog(changelog, version, publishedTag, draft) {
  const [pending, ...releases] = readSections(changelog);
  if (pending?.version !== "Unreleased") {
    throw new Error("CHANGELOG.md must start its release sections with ## [Unreleased]");
  }
  if (releases.some((release) => release.version === version)) {
    throw new Error(`CHANGELOG.md already has a section for ${version}`);
  }
  const publishedIndex = releases.findIndex((release) => `v${release.version}` === publishedTag);
  if (publishedIndex === -1) {
    throw new Error(`CHANGELOG.md has no section for the published baseline ${publishedTag}`);
  }

  const supplement = draft.trimEnd();
  const lines = supplement.split(/\r?\n/);
  if (supplement && (
    !lines.some((line) => line.startsWith("- ")) ||
    lines.some((line) => !/^(?:- |  |$)/.test(line))
  )) {
    throw new Error("changelog draft must contain bullets and optional indented continuations");
  }

  // The newest pending section already carries the reviewed blocks from earlier attempts.
  const previousAttempt = releases.slice(0, publishedIndex)
    .find(({ body }) => body.startsWith(reviewedStart));
  const publishedNotes = new Set(reviewedNotes(releases[publishedIndex].body));
  const carriedNotes = reviewedNotes(previousAttempt?.body ?? "")
    .filter((note) => !publishedNotes.has(note));
  const currentNotes = pending.body ? `${reviewedStart}\n${pending.body}\n${reviewedEnd}` : "";
  const notes = [...carriedNotes, currentNotes, supplement].filter(Boolean).join("\n\n");
  if (!notes) {
    throw new Error("release has no reviewed notes or draft bullets");
  }

  const date = new Date().toISOString().slice(0, 10);
  const heading = `## [${version}] - ${date}`;
  const compare = `[${version}]: https://github.com/liwenka1/deep-code/compare/${publishedTag}...v${version}`;
  const intro = changelog.slice(0, pending.start);
  const history = changelog.slice(releases[0].start).trimEnd();
  return `${intro}## [Unreleased]\n\n${heading}\n\n${notes}\n\n${history}\n${compare}\n`;
}

function releaseNotes(changelog, version) {
  const [pending, ...releases] = readSections(changelog);
  if (pending?.version !== "Unreleased" || pending.body) {
    throw new Error("archive the Unreleased notes before publishing");
  }
  const matches = releases.filter((release) => release.version === version);
  if (matches.length !== 1 || !matches[0].body) {
    throw new Error(`CHANGELOG.md must have one nonempty section for ${version}`);
  }
  return matches[0].body;
}

try {
  const [command, version, publishedTag, draftPath] = process.argv.slice(2);
  if (!/^\d+\.\d+\.\d+$/.test(version)) {
    throw new Error("expected a release version in MAJOR.MINOR.PATCH form");
  }

  const changelog = readFileSync(changelogPath, "utf8");
  if (command === "prepare") {
    const draft = readFileSync(draftPath, "utf8");
    const prepared = prepareChangelog(changelog, version, publishedTag, draft);
    const notes = releaseNotes(prepared, version);
    writeFileSync(changelogPath, prepared);
    process.stdout.write(`${notes}\n`);
  } else if (command === "render") {
    process.stdout.write(`${releaseNotes(changelog, version)}\n`);
  } else {
    throw new Error("use prepare VERSION PREVIOUS_TAG DRAFT or render VERSION");
  }
} catch (error) {
  console.error(`release notes: ${error.message}`);
  process.exitCode = 1;
}
