// Content blocks: text/whitespace/linebreaks/paragraphs/embedded expressions.
#let label(x) = [label: #x]
#let badge(name, n) = [(#name × #n)]
#let plain() = [hello world]
#let multiline() = [first \
  second par]
#let mixed(x) = [value = #x, doubled = #(x * 2)]
#let joined(a, b) = [ [#a] and [#b] ]
