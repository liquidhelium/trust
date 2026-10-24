// Top-level values depending on sys.inputs: the core acceptance workload
// for per-build slots.
#let who = sys.inputs.at("who", default: "anon")
#let shout(w) = upper(w) + "!"
