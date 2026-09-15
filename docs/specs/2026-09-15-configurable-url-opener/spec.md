# Configurable URL opener

`url_opener` names a command for opening PR and Markdown links. reviewr passes the URL as its only
argument. When unset, reviewr uses `open` on macOS or `xdg-open` on Linux.
