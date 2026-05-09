(pair
  key: (string (string_content) @property))

(string) @string

(escape_sequence) @string.escape

(number) @number

[
  (true)
  (false)
] @boolean

(null) @constant.builtin

[
  ","
  ":"
] @punctuation.delimiter

[
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket
