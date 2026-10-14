#!/usr/bin/python
"""Native regression check: video navigation, typing, hover, outside click.
Run in Hyprland with no other Glimpse instance: /usr/bin/python tests/video-navigation.py.
Requires GTK/AT-SPI Python bindings, ffmpeg, cc and libwayland-client.
Uses isolated preferences and synthetic fixtures; downloads nothing.
"""
import gi
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

gi.require_version('Atspi', '2.0')
from gi.repository import Atspi

APP_ID = 'io.github.design_nexus.nixe.Glimpse'
BINARY = os.environ.get('GLIMPSE_TEST_BINARY', str(Path(__file__).resolve().parents[3] / 'target/release/nixe-glimpse'))

def dispatch(code):
    subprocess.run(['hyprctl', 'dispatch', code], check=True, stdout=subprocess.DEVNULL)

def press(key, mods=''):
    for state in ('down', 'up'):
        dispatch(f'hl.dsp.send_key_state({{key="{key}",mods="{mods}",state="{state}"}})')
    time.sleep(.15)

def active():
    return json.loads(subprocess.check_output(['hyprctl', '-j', 'activewindow']))

def follow_mouse():
    return subprocess.check_output(['hyprctl', 'getoption', 'input.follow_mouse'], text=True).splitlines()[0]

assert subprocess.run(['pgrep', '-x', 'nixe-glimpse'], stdout=subprocess.DEVNULL).returncode != 0, 'Close Glimpse before running this test'
original_follow = follow_mouse()
original_cursor = subprocess.check_output(['hyprctl', 'cursorpos'], text=True).strip().split(', ')
with tempfile.TemporaryDirectory(prefix='glimpse-video-navigation-') as temp:
    base = Path(temp)
    pointer = base / 'pointer-input'
    subprocess.run(['cc', str(Path(__file__).with_name('pointer-input.c')), '-lwayland-client', '-lm', '-o', str(pointer)], check=True)
    root = base / 'files'
    root.mkdir()
    video = root / 'navigationfixture-1.mp4'
    subprocess.run(['ffmpeg', '-loglevel', 'error', '-y', '-f', 'lavfi', '-i', 'color=c=blue:s=1280x720:r=24', '-f', 'lavfi', '-i', 'anullsrc=r=48000:cl=stereo', '-t', '8', '-c:v', 'libx264', '-pix_fmt', 'yuv420p', '-c:a', 'aac', str(video)], check=True)
    shutil.copy(video, root / 'navigationfixture-2.mp4')
    (root / 'navigationfixture-3.txt').write_text('Text preview after videos.')
    env = os.environ.copy()
    for name, folder in [('XDG_CONFIG_HOME', 'config'), ('XDG_DATA_HOME', 'data'), ('XDG_STATE_HOME', 'state')]:
        env[name] = str(base / folder)
    env['GLIMPSE_APP_ID'] = APP_ID
    config = base / 'config/nixe-glimpse'
    config.mkdir(parents=True)
    (config / 'settings.toml').write_text(f'roots = [{json.dumps(str(root))}]\npreview = true\nsemantic = false\nsemantic_requested = false\n')
    subprocess.run([BINARY, 'index', 'scan'], env=env, check=True, stdout=subprocess.DEVNULL)
    with (base / 'launcher.log').open('w') as log:
        process = subprocess.Popen([BINARY, '--query', 'navigationfixture'], env=env, stdout=log, stderr=log)
        def nodes():
            def visit(node):
                node.clear_cache()
                return [node] + [child for n in node for child in visit(n)]
            return [n for app in Atspi.get_desktop(0) if app.get_process_id() == process.pid for n in visit(app)]
        def selected():
            return [n.get_index_in_parent() for n in nodes() if n.get_role_name() == 'list item' and n.get_state_set().contains(Atspi.StateType.SELECTED)]
        try:
            for _ in range(40):
                if selected(): break
                time.sleep(.1)
            assert selected() == [0]
            press('Down')
            assert selected() == [1]
            time.sleep(.6)
            assert any(n.get_role_name() == 'slider' and n.get_name() == 'Position' for n in nodes()), 'Video controls did not load'
            assert any(n.get_role_name() == 'entry' and n.get_state_set().contains(Atspi.StateType.FOCUSED) for n in nodes()), 'Search lost focus after video preview loaded'
            window = active()
            dispatch(f'hl.dsp.cursor.move({{x={window["at"][0]+30},y={window["at"][1]+30}}})')
            dispatch(f'hl.dsp.cursor.move({{x={window["at"][0]-20},y={window["at"][1]+30}}})')
            subprocess.run([str(pointer), '2', '2'], check=True)
            time.sleep(.15)
            assert active().get('class') == APP_ID, 'Hover stole keyboard focus from video preview'
            press('Down'); assert selected() == [2]
            press('Up'); assert selected() == [1]
            play = [n for n in nodes() if n.get_role_name() == 'button' and n.get_name() == 'Play']
            assert play and play[-1].get_action_iface().do_action(0)
            time.sleep(1.3)
            assert any(n.get_name() in ('Pause', 'Stop') for n in nodes()), 'Playback did not start'
            for key, index in [('Down', 2), ('Up', 1), ('Up', 0), ('Down', 1)]:
                press(key); assert selected() == [index], 'Navigation stopped after video playback'
            # Controls accept mouse clicks without taking keyboard focus from search.
            assert any(n.get_role_name() == 'entry' and n.get_state_set().contains(Atspi.StateType.FOCUSED) for n in nodes())
            press('x')
            typed = [Atspi.Text.get_text(n, 0, -1) for n in nodes() if n.get_role_name() == 'entry']
            assert any('x' in text.lower() and text != 'navigationfixture' for text in typed), f'Typing stopped after video preview: {typed}'
            subprocess.run([str(pointer), '0', '0', 'click'], check=True)
            process.wait(timeout=5)
            time.sleep(.15)
            assert follow_mouse() == original_follow, 'Normal mouse focus was not restored'
            process = subprocess.Popen([BINARY, '--query', 'navigationfixture'], env=env, stdout=log, stderr=log)
            for _ in range(40):
                if selected(): break
                time.sleep(.1)
            subprocess.run([BINARY, '--plugins'], env=env, check=True, timeout=5)
            for _ in range(30):
                if active().get('title') == 'Glimpse · Plugins': break
                time.sleep(.1)
            assert active().get('title') == 'Glimpse · Plugins', 'Launcher trapped focus when opening Plugins'
            print('PASS: video selection, playback, arrows, typing, hover focus, outside-click dismissal, normal focus and Plugins focus')
        finally:
            if process.poll() is None:
                process.terminate()
                try: process.wait(timeout=5)
                except subprocess.TimeoutExpired: process.kill(); process.wait()
            dispatch(f'hl.dsp.cursor.move({{x={original_cursor[0]},y={original_cursor[1]}}})')
            log.flush()
            if process.returncode not in (0, -15): print((base / 'launcher.log').read_text()[-2000:])
