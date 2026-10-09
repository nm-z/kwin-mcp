# Input timing profile

`input-profile.json` contains aggregate fitted parameters, not recorded text or
participant identifiers. `scripts/fit-input-profile.rb` reproduces it from a
bounded 4 MiB prefix of the published dataset archive. The file records the
sample size, filters, normalization, field order, and source URL.

## Keyboard

Source: Vivek Dhakal, Anna Maria Feit, Per Ola Kristensson, and Antti Oulasvirta,
[Observations on Typing from 136 Million Keystrokes, CHI 2018](https://userinterfaces.aalto.fi/136Mkeystrokes/).
The source dataset permits noncommercial research and project use with attribution.
These fitted data retain that attribution and use restriction.

The fit uses complete participant entries in the prefix, adjacent ASCII keys
within a sentence, holds of 10–500 ms, and press-to-press intervals of 10–2000 ms.
Participants need 100 pairs and mean intervals of 60–153.846 ms. This selects a
fast timing cohort, not the paper's exact WPM cohort. Each participant's timings
are scaled to a 120 ms mean interval, equivalent to 100 WPM at five characters
per word. Each bigram gets a joint lognormal hold/interval fit, retaining their
correlation. Sparse bigrams use the aggregate fit. Word boundaries are fitted
as bigrams containing spaces. Samples can overlap; repeated keys and changes
in Shift state wait for release to preserve the requested text.

This is a prefix sample and a parametric approximation. It does not represent
all 136 million keystrokes or establish that generated input bypasses a site's
verification. The real-session test compares logged timing moments and rollover
against this fitted sample, including a faster setting.

## Pointer and wheel

[MacKenzie and Buxton, CHI 1992, equation 4 and figure 6](https://www.yorku.ca/mack/CHI92.html)
provide the movement-time fit `230 + 166 log2(D/W + 1)` ms and a 64 ms standard
error of estimate. We sample a normal residual at that scale; that residual is
an implementation model, not a claim that the study fitted an individual-trial
normal distribution. `target_width` supplies the smaller target dimension;
its default of 20 pixels is an explicit fallback.

The progress polynomial is `10t³ − 15t⁴ + 6t⁵`, with zero endpoint velocity and
acceleration. [Fischer et al., 2020, section 8.1](https://arxiv.org/abs/2002.11596)
discuss the minimum-jerk model and its limits for corrective motion. Our path
adds a sampled lateral bend and, beyond 200 pixels, a short final correction.
Bend amplitude, correction distance, and the 85/15 duration split are engineering
choices. Reports run at up to 125 Hz, with late reports skipped rather than
replayed in a burst.

Click holds and wheel intervals reuse the keyboard timing scale. They are not
separate empirical mouse-button or wheel fits. Smooth wheel ticks distribute
15 pixels across three reports; discrete ticks preserve value120 units.
Every generated event goes through the session's persistent input log.
