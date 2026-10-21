// 依赖 sys.inputs 的顶层值:per-build 槽板的核心验收负载
// (见 _things/DESIGN-per-build.md §4)。
#let who = sys.inputs.at("who", default: "anon")
#let shout(w) = upper(w) + "!"
