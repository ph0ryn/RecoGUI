# Reco Evaluation

## What an accuracy claim requires

Accuracy must be evaluated against a human-reviewed reference transcript. A transcript produced by
Reco, another ASR system, or an earlier run is a pseudo-reference and cannot establish absolute CER
or WER.

Use the same ASR model, language, audio, and normalization settings when evaluating pipeline changes.
This isolates improvements in segmentation, context preservation, token handling, and result
retention from gains caused by replacing the model.

For Japanese lecture audio, character error rate (CER) is usually the most direct primary metric.
Define and version the normalization applied before scoring, including Unicode normalization,
whitespace, punctuation, numeral forms, and filler-word policy. Report raw and normalized results when
normalization could hide meaningful errors.

Reco includes an exact Levenshtein CER scorer for UTF-8 text files. Its bit-parallel alignment engine
keeps long lecture transcripts practical without changing the metric:

```sh
uv run reco-evaluate reference.txt hypothesis.txt
uv run reco-evaluate --raw reference.txt hypothesis.txt
```

The default `nfkc-casefold-strip-cpz-v1` profile applies NFKC compatibility normalization and case
folding, then removes whitespace, punctuation, and control characters. It preserves kana distinctions,
the long-vowel mark, symbols, and ordinary letters and numbers. `--raw` uses the `raw-v1` profile and
keeps every original character. An empty reference is an error because CER would be undefined.

The command prints the normalization profile, exact total edit distance, deterministic insertion,
deletion, and substitution counts, normalized reference and hypothesis lengths, and CER. The counts
describe one deterministic optimal alignment. Multiple equally optimal alignments can have different
operation breakdowns, so keep the dependency lock fixed when comparing those counts; total edit
distance and CER remain invariant. CER may exceed 100% when insertions outnumber reference
characters.

Do not report only a corpus-wide aggregate. At minimum retain per-file CER, total reference
characters, insertions, deletions, substitutions, empty segments, and token-limit warnings. Inspect
the worst regressions manually.

## Segmentation correctness

Deterministic tests should enforce these invariants independently of ASR quality:

- input chunks have contiguous 16 kHz sample positions;
- every emitted segment has a positive sample range;
- transcript indexes are contiguous and ordered;
- transcript sample ranges do not overlap;
- an adaptive split partitions its crossing frame exactly once;
- pre-roll is present when speech starts after buffered audio;
- a late silence event cannot create a zero-duration segment;
- EOF and `Ctrl+C` finalize an open speech segment;
- a partial final VAD frame does not add zero padding to emitted audio; and
- failures either join the ASR worker or reach the documented bounded daemon-abandonment path without
  deadlocking the CLI.

These checks catch audio corruption that a fluent-looking transcript can conceal.

## Performance measurement

Measure cold startup and steady pipeline work separately:

- model load time;
- command and pipeline wall time;
- media duration;
- pipeline RTF and decode RTF;
- per-segment decode and queue-wait percentiles;
- maximum ASR queue depth;
- segment count and split-reason distribution;
- token-limit retries and unresolved token-limit warnings;
- peak resident memory; and
- terminal progress event count when UI performance is under test.

RTF below `1.0` means the measured path completed faster than the source duration. Always name the
path being timed: an end-to-end CLI RTF includes different work from decode RTF.

For repeatable comparisons:

1. record the commit, model revision, language, hardware, operating system, dependency lock, and VAD
   configuration;
2. use the same ordered audio manifest and verify file hashes;
3. separate the first cold-model run from later warm runs;
4. run multiple repetitions and report a median plus dispersion, not only the best run;
5. benchmark with recording disabled unless SQLite cost is the subject of the test;
6. capture stdout outside the timed region when measuring the core pipeline; and
7. compare segment diagnostics before attributing a timing change to ASR.

Personal audio corpora should remain outside the repository. A benchmark manifest may contain stable
relative names, hashes, durations, and human-reference locations without committing the audio itself.

## Regression workflow

Run the repository checks first:

```sh
uv run task format
uv run task lint
uv run task typecheck
uv run pytest
uv build
```

Then evaluate representative real audio with the same default model. A change is ready only when:

- structural invariants remain satisfied;
- no worker, queue, or recording lifecycle failure is introduced;
- performance changes are measured on the same corpus and hardware;
- accuracy is unchanged or improved on human references, within a declared tolerance; and
- any trade-off is visible in per-file and per-segment diagnostics.

Pseudo-references are still useful for detecting large unexpected drift, but label those comparisons
as stability checks. They must not be presented as ground-truth accuracy.
