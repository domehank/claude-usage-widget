# Claude usage widget

Two small bars in the Windows taskbar that show how much of your Claude usage limits you have used:

```
5h ──────────  0%
W  ━━━━━━━──── 70%
```

- **5h**: the 5-hour session limit
- **W**: the weekly limit

The bars turn orange at 75% and red at 90%.

- **Hover** over the widget to see the exact numbers, how many tokens you have used in each
  window (for example `15.7k`, with an estimated maximum once a window passes 10%), and when
  each limit resets.
- **Drag** it left or right (grab it anywhere in its area) to move it along the taskbar. It remembers where you put it.
- **Right-click** it for *Reset position* and *Exit*.

## What you need

- Windows 10 or 11, with the taskbar at the bottom of the screen
- [Claude Code](https://docs.claude.com/en/docs/claude-code/overview) with a **Claude Pro or Max** plan.
  Claude Code only reports usage limits for these plans.

## How it works

The widget doesn't connect to the internet or ask for a login. It reads a small file,
`%USERPROFILE%\.claude\usage.json`, and redraws whenever the file changes.

Claude Code writes that file through its **status line**, the line of text it shows under
the chat box. Claude Code runs a script after each reply and passes it your usage numbers.
The script in [`statusline/usage-statusline.ps1`](statusline/usage-statusline.ps1) saves
the numbers for the widget. So the widget updates while you use Claude Code. When you aren't
using it, the widget shows the last known numbers and switches to 0% once a limit has reset.

The token counts in the hover text come from Claude Code's own session logs in
`%USERPROFILE%\.claude\projects\`. Each reply records the tokens it used, and the widget adds
them up for the current 5-hour and weekly windows. Two things to know:

- They only include Claude Code on **this PC**, not claude.ai or Claude Code on other computers.
- They count input, output and cache-write tokens, but not **cache reads**. Every reply
  re-reads the whole conversation from cache, so including them would make the numbers huge.

Once a window is at least 10% used, the hover text also shows an **estimated maximum**, for
example `188k / ~1.9M tokens`. Claude doesn't publish token limits, so the widget works it out
as *tokens used ÷ percentage used*. Treat it as a rough guide:

- Limits are weighted by model, so the estimate is lower when you use Opus a lot and higher with Sonnet.
- If you also use claude.ai or Claude Code on another computer, the percentage goes up but this
  PC's token count doesn't, so the estimate comes out too low.
- Below 10% the whole-number percentage is too rough to estimate from, so no maximum is shown.

## Install

### Step 1: Download the widget

1. Open the [Releases page](https://github.com/domehank/claude-usage-widget/releases/latest) and download `claude-usage-widget.exe`.
2. Make a folder for it, for example `C:\Users\<you>\Apps\`, and move the file there.
3. Double-click it. The bars appear in the taskbar, next to the clock and tray icons.

Windows may show a blue **"Windows protected your PC"** box, because the program isn't
signed with a paid certificate. Click **More info**, then **Run anyway**. You only need to do
this once. If you'd rather not run a downloaded program, you can build it yourself (see
[Build from source](#build-from-source)).

The bars stay empty until you finish step 2.

### Step 2: Connect it to Claude Code

1. Download [`usage-statusline.ps1`](statusline/usage-statusline.ps1) (click the file, then
   the download button) and save it to `C:\Users\<you>\.claude\`.
2. Open `C:\Users\<you>\.claude\settings.json` in Notepad. Create the file if it doesn't exist.
3. Add the `statusLine` part below. Replace `<you>` with your Windows user name and use
   forward slashes `/` in the path.

   If the file was empty, the whole file looks like this:

   ```json
   {
     "statusLine": {
       "type": "command",
       "command": "powershell -NoProfile -ExecutionPolicy Bypass -File C:/Users/<you>/.claude/usage-statusline.ps1"
     }
   }
   ```

   If the file already has settings, add the `"statusLine": { ... }` block inside the outer
   `{ }`, with a comma between it and the setting before it.
4. Start Claude Code (or restart it) and send any message. Once Claude replies, the bars
   fill in. You'll also see the model name and folder under the chat box.

**Already have a status line?** Claude Code only runs one status line command. Copy the part
of `usage-statusline.ps1` marked `# 1. Save the usage numbers` into your own script.

### Step 3: Start it when Windows starts (optional)

1. Press **Win + R**, type `shell:startup` and press **Enter**. A folder opens.
2. Right-click `claude-usage-widget.exe` in your Apps folder, choose **Show more options →
   Create shortcut**, and move the new shortcut into the Startup folder.

## Uninstall

1. Right-click the widget and choose **Exit**.
2. Delete the shortcut from the Startup folder (`shell:startup`) and delete `claude-usage-widget.exe`.
3. Remove the `statusLine` block from `settings.json`, and delete `usage-statusline.ps1`,
   `usage.json` and `usage-widget.json` from `C:\Users\<you>\.claude\`.

## Troubleshooting

| Problem | What to do |
| --- | --- |
| The bars show `–` | No usage data yet. Check step 2, then send a message in Claude Code. If you use an API key instead of a Pro/Max plan, Claude Code has no limits to report. |
| The widget isn't visible | It may be behind other taskbar icons. Right-click where it should be, or exit it in Task Manager and start it again. Then right-click it and choose **Reset position**. |
| The numbers look old | They only update while Claude Code is running. Hover to see when they were last updated. |
| It disappeared after Explorer crashed | It comes back by itself within about 15 seconds. |

## Build from source

1. Install Rust from [rustup.rs](https://rustup.rs/). When the installer offers to install the
   Visual Studio build tools, choose that option (1). Rust needs them to build Windows programs.
2. Download this project (green **Code** button → **Download ZIP**, then unzip it), open a terminal in that folder and run:

   ```
   cargo build --release
   ```

   The first build downloads the libraries it needs, so it takes a minute or two.
3. The program is at `target\release\claude-usage-widget.exe`. If an older copy is running,
   exit it first (right-click → Exit).

Files the widget uses, all in `%USERPROFILE%\.claude\`:

- `usage.json`: usage numbers written by the status line script
- `usage-widget.json`: the widget's saved position
