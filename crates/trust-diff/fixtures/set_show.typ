// set/show 规则(M2.1):求值期只构造 Styles/Recipe 并包装尾部内容,
// 变换由 typst realize/layout 执行(见 _things/DESIGN-per-build.md)。
#let styled = [A #set text(size: 14pt); B]
#let cond(flag) = [x #set text(weight: "bold") if flag; y]
#let shown = [x #show rect: box; #rect()]
