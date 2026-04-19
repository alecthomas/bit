; Inject shell highlighting into heredocs
((heredoc) @injection.content
 (#set! injection.language "bash")
 (#set! injection.include-children))
