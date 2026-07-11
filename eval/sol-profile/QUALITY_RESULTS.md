# GPT-5.6 Sol quality results

Model: `gpt-5.6-sol` through the Codex Responses provider. Image calls bypassed
pxpipe.

## Production 5×8 profile

Production uses Spleen 5×8, 152 columns, max height 1932, monochrome AA, IDS
rows, and the adjacent text factsheet.

| test | text | production image | notes |
|---|---:|---:|---|
| novel arithmetic, N=100 | 100/100 | 98/100 | pure image 96/100 |
| gist recall | not measured | 79/93 completed | one six-probe session failed at the gateway |
| state tracking | not measured | 18/18 | no transport errors |
| never-stated guards | not measured | 4/15 completed confabulated | one guard shared the failed session |
| dense 12-char hex | not run in this harness | 0/15 | all calls completed |

Matched arithmetic usage was 5,300 text input tokens and 7,000 production-image
input tokens, **+32.1%**. The README rounds this to **+32%**. Short prompts are
not a compression win, even when the model reads them.

The arithmetic receipt was reconstructed from the retained N=100 run log and
provider usage. Earlier JSON metadata recorded the resolved JetBrains profile
even though the run selected the Spleen candidate. The request bytes and run log
identify the actual 5×8 render; this document records that correction instead
of presenting the stale recipe field as evidence.


## Decision

Sol remains opt-in. Its arithmetic and state results are strong, but gist,
abstention, and dense exact recall do not match Fable. Sibling `gpt-5.6-*`
models do not inherit Sol's profile or allowlist.

Receipts:

- `novel-arithmetic-spleen5x8-results.json`
- `gist-recall-results.json`
- `verbatim-hex-results.json`
