// Set/show rules: evaluation only constructs Styles/Recipes and wraps the
// tail content; the transformation runs in typst's realize/layout.
#let styled = [A #set text(size: 14pt); B]
#let cond(flag) = [x #set text(weight: "bold") if flag; y]
#let shown = [x #show rect: box; #rect()]
