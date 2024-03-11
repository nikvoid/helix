([(line_comment) (block_comment)] @injection.content
 (#set! injection.language "comment"))

((macro_invocation
   macro:
     [
       (scoped_identifier
         name: (_) @_macro_name)
       (identifier) @_macro_name
     ]
   (token_tree) @injection.content)
 (#eq? @_macro_name "html")
 (#set! injection.language "html")
 (#set! injection.include-children))

((macro_invocation
   macro:
     [
       (scoped_identifier
         name: (_) @_macro_name)
       (identifier) @_macro_name
     ]
   (token_tree) @injection.content)
 (#eq? @_macro_name "slint")
 (#set! injection.language "slint")
 (#set! injection.include-children))

((macro_invocation
  (token_tree) @injection.content)
 (#set! injection.language "rust")
 (#set! injection.include-children))

((macro_rule
  (token_tree) @injection.content)
 (#set! injection.language "rust")
 (#set! injection.include-children))

(call_expression
  function: (scoped_identifier
    path: (identifier) @_regex (#eq? @_regex "Regex")
    name: (identifier) @_new (#eq? @_new "new"))
  arguments: (arguments (raw_string_literal) @injection.content)
  (#set! injection.language "regex"))

(call_expression
  function: (scoped_identifier
    path: (scoped_identifier (identifier) @_regex (#eq? @_regex "Regex") .)
    name: (identifier) @_new (#eq? @_new "new"))
  arguments: (arguments (raw_string_literal) @injection.content)
  (#set! injection.language "regex"))

; Highlight SQL in `sqlx::query!()`, `sqlx::query_scalar!()`, and `sqlx::query_scalar_unchecked!()`
(macro_invocation
  macro: (scoped_identifier
    path: (identifier) @_sqlx (#eq? @_sqlx "sqlx")
    name: (identifier) @_query (#match? @_query "^query(_scalar|_scalar_unchecked)?$"))
  (token_tree
    ; Only the first argument is SQL
    .
    [(string_literal) (raw_string_literal)] @injection.content
  )
  (#set! injection.language "sql"))

; Highlight SQL in `sqlx::query_as!()` and `sqlx::query_as_unchecked!()`
(macro_invocation
  macro: (scoped_identifier
    path: (identifier) @_sqlx (#eq? @_sqlx "sqlx")
    name: (identifier) @_query_as (#match? @_query_as "^query_as(_unchecked)?$"))
  (token_tree
    ; Only the second argument is SQL
    .
    ; Allow anything as the first argument in case the user has lower case type
    ; names for some reason
    (_)
    [(string_literal) (raw_string_literal)] @injection.content
  )
  (#set! injection.language "sql"))

; highlight maud's html! macro as css (best fit?)
; (macro_invocation
;   macro: (identifier) @_html (#eq? @_html "html")
;   (token_tree) @injection.content
;   (#set! injection.language "css")
;   (#set! injection.include-children))

; highlight arbitrary literal
; (
;   [(block_comment) (line_comment)] @injection.language
;   [(string_literal) (raw_string_literal)] @injection.content
; )

; highlight arbitrary string literal (only by block comment, increases INS perf)
(
  [(block_comment)] @injection.language
  [(string_literal) (raw_string_literal)] @injection.content
)

; Highlight interpolation macros (currenty broken and highlights all)
; good candidates: 
;   cue, perl, fish(--), erlang(-), css(-), openscad(-), protobuf,
;   svelte, 
;(macro_invocation
;  macro: ((identifier) @_macro (#any-of? @_macro
;    "print" "println" "anyhow" "bail" "panic" "write" "writeln" "format"))
;  (token_tree
;    [(string_literal) (raw_string_literal)] @injection.content
;  )
;  (#set! injection.language "svelte"))
