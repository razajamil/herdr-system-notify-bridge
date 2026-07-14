-- HerdrFocus: URL-scheme handler for herdrfocus://focus/<pane-id>
--
-- Registered (via install.sh) as the owner of the `herdrfocus://` scheme. When
-- a notification wired with `-open herdrfocus://focus/<pane>` is clicked, macOS
-- launches this app with the URL; it focuses that herdr pane and raises kitty.
--
-- The pane id is taken as everything after the last "/", because pane ids
-- contain ":" (e.g. w1:p4) which is not safe in a URL authority.
on open location this_URL
	set AppleScript's text item delimiters to "/"
	set parts to text items of this_URL
	set paneId to last item of parts
	set AppleScript's text item delimiters to ""
	if paneId is "" then return

	set homePath to POSIX path of (path to home folder)
	set herdr to homePath & ".local/bin/herdr"
	set logf to homePath & ".config/herdr/herdrfocus.log"

	do shell script quoted form of herdr & " agent focus " & quoted form of paneId & ¬
		" >> " & quoted form of logf & " 2>&1" & ¬
		"; /bin/date '+%H:%M:%S focused' >> " & quoted form of logf & ¬
		"; echo " & quoted form of paneId & " >> " & quoted form of logf & ¬
		"; /usr/bin/open -b net.kovidgoyal.kitty"
end open location
