(array
  "[" @open
  "]" @close)

(object
  "{" @open
  "}" @close)

(("\"" @open
  "\"" @close)
  (#set! rainbow.exclude))
