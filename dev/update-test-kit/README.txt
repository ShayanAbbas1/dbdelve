DBDelve update test kit
=======================

This kit tests DBDelve updating itself on Windows: the "Update and restart"
button, the administrator prompt for an all-users install, and DBDelve
reopening afterwards as an ordinary (not administrator) program. It needs
nothing installed. Everything in it is a test build: started by
Start-Test.cmd, it looks for updates only in this kit. Started any other
way (the installer's "Launch DBDelve", the Start menu), it asks GitHub
like a normal build.

What is in it:
  old\dbdelve-{OLD}-windows-{ARCH}-setup.exe   the installer of the old build
  old\dbdelve-{OLD}-windows-{ARCH}.zip         the portable old build
  feed\                     the "new" release, {NEW}, with its signatures
  update-key.pub            the throwaway key those signatures were made with
  Start-Test.cmd            serves feed\ and starts DBDelve pointed at it

Before you start
----------------
1. Before extracting the kit's zip, right-click it > Properties, tick
   "Unblock" at the bottom and press OK. Then extract it (right-click >
   Extract All). Without this, Windows warns about every program in it.
2. Nothing here is signed. If Windows still shows "Windows protected your
   PC", click "More info", then "Run anyway". The administrator prompt will
   name an unknown publisher; that is expected.
3. Start-Test.cmd must be started normally (double-click, or from an
   ordinary Command Prompt), never "as administrator". Its window serves the
   update: leave it open until a pass is finished, then press Ctrl+C in it
   (answer Y if asked) or close it.
4. DBDelve uses its usual folders: settings in %APPDATA%\dbdelve, the log in
   %LOCALAPPDATA%\dbdelve (one file per day, dbdelve.<date>.log).

How to tell which version is running
------------------------------------
- Before the update the titlebar shows a pill "DBDelve {NEW} available".
  After it, the pill is gone.
- Press Ctrl+Shift+P, type "Copy diagnostics", press Enter and paste into
  Notepad: the first line says "DBDelve: {OLD}" or "DBDelve: {NEW}". (There
  is no About window.)
- The log has a line "dbdelve started version=..." for every start.

Pass A: installed just for you
------------------------------
1. Run old\dbdelve-{OLD}-windows-{ARCH}-setup.exe and choose "Install for me
   only". On the last page untick "Launch DBDelve" (or close DBDelve if it
   opens).
2. Double-click Start-Test.cmd. DBDelve opens and, within a few seconds, the
   titlebar shows "DBDelve {NEW} available".
3. Click the pill, then "Update and restart". The pill counts up while it
   downloads, then reads "Installing...".
4. Expected: DBDelve closes and reopens by itself, without any prompt, the
   pill is gone, and Copy diagnostics says {NEW}.
5. Close DBDelve and stop Start-Test (Ctrl+C).

Pass B: installed for all users
-------------------------------
1. Uninstall DBDelve: Settings > Apps > Installed apps (Apps & features on
   Windows 10) > DBDelve > Uninstall.
2. Run the old setup again and choose "Install for all users". Approve the
   administrator prompt. Untick "Launch DBDelve" at the end.
3. Double-click Start-Test.cmd and wait for the pill.
4. Click the pill > "Update and restart". An administrator prompt appears.
   Click "No".
5. Expected: DBDelve keeps running. The pill still says "DBDelve {NEW}
   available"; clicked, it shows "The update was cancelled before anything
   was installed." and "Try again".
6. Click "Try again" and this time approve the prompt.
7. Expected: DBDelve closes and reopens as {NEW}.
8. Check it reopened as an ordinary program: open Task Manager (Ctrl+Shift+
   Esc) > Details, right-click a column heading > Select columns, tick
   "Elevated", OK. The dbdelve.exe row must say "No" in that column.
9. Close DBDelve and stop Start-Test.

Pass C: portable
----------------
1. Right-click old\dbdelve-{OLD}-windows-{ARCH}.zip > Extract All, and
   extract it to a folder of your own, for example
   C:\Users\<you>\Desktop\dbdelve-portable.
2. In the kit folder, click the address bar, type cmd and press Enter. In
   the window that opens, type (with your folder):
     Start-Test -Portable "C:\Users\<you>\Desktop\dbdelve-portable"
3. Click the pill > "Update and restart".
4. Expected: DBDelve reopens as {NEW} from the same folder, with no prompt.
   The folder keeps its name.
5. Close DBDelve and stop Start-Test.

If something goes wrong
-----------------------
Write down what you clicked and what you saw (a screenshot of the pill's
menu helps), and send:
- the newest dbdelve.<date>.log from %LOCALAPPDATA%\dbdelve, and panic.log
  if there is one there;
  The log names folders on this PC, your Windows user name among them;
  blank out anything you would rather not share.
- the text in the Start-Test window;
- for passes A and B, a listing of the install folder. In a Command Prompt:
    dir /s /a "%LOCALAPPDATA%\Programs\DBDelve" > "%USERPROFILE%\Desktop\listing.txt"
  for pass A, or for pass B:
    dir /s /a "%ProgramFiles%\DBDelve" > "%USERPROFILE%\Desktop\listing.txt"
  and send listing.txt from your desktop.

Cleaning up
-----------
1. Uninstall DBDelve: Settings > Apps > Installed apps (Apps & features on
   Windows 10) > DBDelve > Uninstall.
2. Delete the portable folder from pass C, and the kit folder.
3. Only if this PC had no DBDelve before the test: delete %APPDATA%\dbdelve
   and %LOCALAPPDATA%\dbdelve (paste each into File Explorer's address bar
   to find them). If you use DBDelve here, leave them: %APPDATA%\dbdelve
   holds your saved connections and queries.
