" An ergon outline: a header, then one row per task: nine cells (slug,
" status, ~status, priority, ~priority, size, ~size, deps, ~blocked) and the
" title. `-` is an empty cell; `~` cells are calculated.
if exists("b:current_syntax")
  finish
endif

syn match ergonSlug /^\S\+/
syn match ergonEmpty /\s\zs-\ze\s\s/
syn match ergonInferred /\s\zs\~\S\+/
syn match ergonBlocked /\s\zs\~blocked\>/
syn match ergonPriority /\s\zsp[0-4]\ze\s/
syn match ergonTodo /^\S\+\s\+\zstodo\>/
syn match ergonInProgress /^\S\+\s\+\zsin_progress\>/
syn match ergonLanding /^\S\+\s\+\zslanding\>/
syn match ergonMarker /\s\zs-\ze \S/
" Finished tasks are dimmed whole, by their actual status. Defined last, so
" it wins over the matches above. The highlight groups are set in
" plugin/ergon.lua.
syn match ergonFinished /^\S\+\s\+\S\+\s\+\~\(done\|cancelled\)\>.*$/
syn match ergonHeader /\%1l^#.*$/

let b:current_syntax = "ergon"
