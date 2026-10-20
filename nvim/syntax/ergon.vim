" An ergon outline: `<slug> [<box>] <title> [!p<N>] [deps:<slug>,...]`.
if exists("b:current_syntax")
  finish
endif

syn match ergonHeader /\%1l^#.*$/
syn match ergonSlug /^\s*\zs[34679ADEFGHJLMNQRTYabcdefghijkmnpqrtuvwxy]\{4}\ze\s/
syn match ergonPriority /\s\zs!p[0-4]\ze\(\s\|$\)/
syn match ergonDeps /\s\zsdeps:\S*/
syn match ergonTodo /^\s*\(\S\{4} \)\=\zs\[ \]/
syn match ergonInProgress /^\s*\(\S\{4} \)\=\zs\[\~\]/
syn match ergonLanding /^\s*\(\S\{4} \)\=\zs\[>\]/
" Finished tasks are dimmed from the box on. The highlight groups are set in
" plugin/ergon.lua.
syn match ergonDone /^\s*\(\S\{4} \)\=\zs\[[xX]\].*$/
syn match ergonCancelled /^\s*\(\S\{4} \)\=\zs\[-\].*$/

let b:current_syntax = "ergon"
