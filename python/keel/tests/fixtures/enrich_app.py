"""Full-pipeline app: imports a target module (wrapped by the hook under
`keel run`) and calls one of its functions. stdout carries only the program's
own output, so the banner (stderr) never contaminates it."""

import sample_targets

print("enriched", sample_targets.enrich_a(41))
