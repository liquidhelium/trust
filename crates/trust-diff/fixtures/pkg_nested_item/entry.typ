// In-body nested import items (`import <module value>: a.b as c`) must
// walk the full item path component by component, like typst-eval.
#import "mid.typ" as om

#let renamed() = {
  import om: a.b as bb
  bb
}

#let plain() = {
  import om: a.b
  b
}
