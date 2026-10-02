" Syntax highlighting for Cagara (`.cagara`).

if exists("b:current_syntax")
  finish
endif

syn keyword cagaraImport   import as
syn keyword cagaraKeyword  sql
syn keyword cagaraBoolean  true false
syn keyword cagaraType     query expr agg win maybe list int float string bool date timestamp sortkey winspec bound frame

syn match   cagaraComment  "#.*$" contains=@Spell
syn region  cagaraString   start=+"+ skip=+\\.+ end=+"+ contains=cagaraEscape,cagaraPlaceholder
syn match   cagaraEscape   +\\.+ contained
syn match   cagaraPlaceholder "\$\d\+" contained

syn match   cagaraColumn   "\.[<>]\=\h\w*"
syn match   cagaraFloat    "\<\d\+\.\d\+\([eE][+-]\=\d\+\)\=\>"
syn match   cagaraNumber   "\<\d\+\>"
syn match   cagaraOpName   "\<_[-+*/%<>=!&|$?.]\+_"
syn match   cagaraArrow    "=>\|->"
syn match   cagaraOperator "<?>\|<?\|?>\|??\|&[=?*.+-]\|>>>\|&&\|||\|==\|!=\|<>\|<=\|>=\|[&$?<>+*/%|-]"
" A top-level name at column 0, before its signature or body.
syn match   cagaraDef      "^\(\h\w*\|_[-+*/%<>=!&|$?.]\+_\)\ze\s*\(:\|=[^=]\)"

hi def link cagaraImport      Include
hi def link cagaraKeyword     Keyword
hi def link cagaraBoolean     Boolean
hi def link cagaraType        Type
hi def link cagaraComment     Comment
hi def link cagaraString      String
hi def link cagaraEscape      SpecialChar
hi def link cagaraPlaceholder Special
hi def link cagaraColumn      Identifier
hi def link cagaraFloat       Float
hi def link cagaraNumber      Number
hi def link cagaraOpName      Function
hi def link cagaraArrow       Operator
hi def link cagaraOperator    Operator
hi def link cagaraDef         Function

let b:current_syntax = "cagara"
