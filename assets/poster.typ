#import "@preview/codly:2.0.0" as codly

#set page(width: 760pt, height: auto, margin: 16pt, fill: white)
#set text(font: "DejaVu Sans Mono", size: 11pt, fill: rgb("#1f2328"))

#show: codly.theme("github-light")
#show: codly.set_(padding: 6pt, radius: 4pt)

#align(center)[
  #text(size: 20pt, weight: "bold")[trust]
  #h(8pt)
  #text(size: 12pt, fill: rgb("#656d76"))[#underline("T")ypst, compiled to #underline[RUST]]
]

#v(10pt)

#grid(
  columns: (1fr, 1.7fr),
  column-gutter: 14pt,
  codly.new(
    ```typ
    #let add(a, b) = a + b

    #let greet = [Hello]
    ```,
    file: "snippet.typ",
    highlights: (
      (line: 1, end:14, fill: rgb("#0969da")),
      (line: 1, start: 18, fill: rgb("#9a6700")),
      (line: 3, fill: rgb("#8250df")),
    ),
  ),
  codly.new(
    ```rs
    fn f_v0_add(
      engine: &mut Engine,
      context: Tracked<Context>,
      __args: &mut Args,
    ) -> SourceResult<Value> {
      let mut a = rt::param_pos(__args, "a", span)?;
      let mut b = rt::param_pos(__args, "b", span)?;
      rt::finish(__args, span)?;
      ops::add(a.clone(), b.clone()).at(sp(0))?
    }

    let v = Value::Content(
      rt::text("Hello", sp(1)),
    );
    rt::fill(&__st.snippet.V1_greet, v)?;
    ```,
    file: "generated excerpt",
    highlights: (
      (line: 1, end: 12, fill: rgb("#0969da")),
      (line: 9, start:3, fill: rgb("#9a6700")),
      (line: 13, fill: rgb("#8250df")),
    ),
    annotations: (
      (start: 6, end: 9, content: [`a + b`], numbering: none),
      (start: 12, end: 15, content: [`[Hello]`], numbering: none),
    ),
    callouts: (
      (
        line: 9,
        body: [`typst::foundations::ops::add`],
        pointer: 10,
        bubble-fill: aqua.lighten(80%),
        pointer-align: "start",
        bubble-stroke:aqua,
      ),
    ),
  ),
)
