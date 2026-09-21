#!/usr/bin/env python3
"""从图标母版生成两端的全部平台图标。

母版：`assets/icons/app-icon-1024.png`（1024×1024 RGBA，深蓝圆角徽章 + 青色麦克风/声波/光标）
派生：PC 端交给 Tauri 官方 `icon` 子命令；Android 端由本脚本用 ffmpeg 生成。

母版是怎么来的（若要重新导出，按这个流程；原设计图不入库，避免仓库里存大图）：
  1. 原图（2048²、白底）里徽章的严格边界实测为 crop=1310:1240:371:418；
  2. `scale=1024:1024`：同时把"徽章比高度宽 5.3%"的生成瑕疵校正为正方形；
  3. 白底/中性灰外缘键出为透明（保留圆角与描边的抗锯齿），得到母版。

用法：
    python3 scripts/build_icons.py            # 生成 PC + Android 全部图标
    python3 scripts/build_icons.py --check    # 只校验母版与工具可用性
"""
import argparse
import glob
import os
import re
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MASTER = os.path.join(ROOT, 'assets/icons/app-icon-1024.png')
ANDROID_RES = os.path.join(ROOT, 'app/android/app/src/main/res')
N = 1024
# Android 自适应图标：108dp 画布，内容安全区 66.7%
DENSITIES = {
    'mdpi': (48, 108, 72),
    'hdpi': (72, 162, 108),
    'xhdpi': (96, 216, 144),
    'xxhdpi': (144, 324, 216),
    'xxxhdpi': (192, 432, 288),
}


def ff(args, inp=None, expect=True):
    p = subprocess.run(['ffmpeg', '-v', 'error'] + args, input=inp, capture_output=True)
    if expect and inp is not None and p.returncode != 0:
        sys.exit('ffmpeg 失败: ' + p.stderr.decode()[:300])
    return p.stdout


def png_size(path):
    out = subprocess.run(['ffprobe', '-v', 'error', '-select_streams', 'v:0', '-show_entries',
                          'stream=width,height', '-of', 'csv=p=0', path],
                         capture_output=True).stdout.decode().strip()
    return tuple(int(v) for v in out.split(','))


def decode(path, size):
    return bytearray(ff(['-i', path, '-f', 'rawvideo', '-pix_fmt', 'rgba', '-s', f'{size}x{size}', '-']))


def to_png(rgba, size, path):
    ff(['-f', 'rawvideo', '-pix_fmt', 'rgba', '-s', f'{size}x{size}', '-i', '-', '-y', path], inp=rgba)
    print('  →', os.path.relpath(path, ROOT))


def scale_rgba(buf, src_size, dst_size):
    """带 alpha 的等比缩放（lossless 前先走一次 ffmpeg 的高质量重采样）"""
    out = ff(['-f', 'rawvideo', '-pix_fmt', 'rgba', '-s', f'{src_size}x{src_size}', '-i', '-',
              '-vf', f'scale={dst_size}:{dst_size}:flags=lanczos',
              '-f', 'rawvideo', '-pix_fmt', 'rgba', '-'], inp=bytes(buf))
    return bytearray(out)


def crop_scale_rgba(buf, src_size, x0, y0, cw, ch, tw, th):
    out = ff(['-f', 'rawvideo', '-pix_fmt', 'rgba', '-s', f'{src_size}x{src_size}', '-i', '-',
              '-vf', f'crop={cw}:{ch}:{x0}:{y0},scale={tw}:{th}:flags=lanczos',
              '-f', 'rawvideo', '-pix_fmt', 'rgba', '-'], inp=bytes(buf))
    return bytearray(out)


def circle_mask(buf, size):
    c = (size - 1) / 2
    out = bytearray(buf)
    for y in range(size):
        for x in range(size):
            k = max(0.0, min(1.0, c - ((x - c) ** 2 + (y - c) ** 2) ** 0.5 + 0.5))
            o = (y * size + x) * 4
            out[o + 3] = int(out[o + 3] * k)
    return out


def navy_reference(master):
    """徽章底色是横向渐变：取左右内缘的中位数，作为逐列参考色"""
    def median_strip(fx0, fx1):
        vals = []
        for y in range(int(N * .30), int(N * .70), 4):
            for x in range(int(N * fx0), int(N * fx1), 4):
                o = (y * N + x) * 4
                vals.append(tuple(master[o:o + 3]))
        vals.sort(key=sum)
        return vals[len(vals) // 2]
    l, r = median_strip(.06, .10), median_strip(.90, .94)
    return tuple(round(l[i] + (r[i] - l[i]) * 0.5) for i in range(3)), l, r


def build_android(master):
    mid, l, r = navy_reference(master)
    print('  Android 自适应背景色：#%02X%02X%02X' % mid)
    # 前景层：把图形从底色里抠出来（逐列参考色，抑制渐变造成的误判）
    fg = bytearray(N * N * 4)
    minx, maxx, miny, maxy = N, 0, N, 0
    for y in range(N):
        for x in range(N):
            o = (y * N + x) * 4
            rr, gg, bb, aa = master[o:o + 4]
            if not aa:
                continue
            t = x / (N - 1)
            nv = tuple(round(l[i] + (r[i] - l[i]) * t) for i in range(3))
            d = max(abs(rr - nv[0]), abs(gg - nv[1]), abs(bb - nv[2]))
            ga = int(max(0.0, min(1.0, (d - 6) / 30)) * 255)
            if ga:
                fg[o:o + 4] = bytes((rr, gg, bb, ga))
                # 徽章外缘那圈描边与底色差 ~25，不算图形：只在中央窗口内取包围盒
                if ga > 25 and .12 * N < x < .88 * N and .12 * N < y < .88 * N:
                    minx, maxx, miny, maxy = min(minx, x), max(maxx, x), min(miny, y), max(maxy, y)
    gw, gh = maxx - minx + 1, maxy - miny + 1
    if not (0 < gw < N and 0 < gh < N):
        sys.exit(f'前景抠图包围盒异常: {gw}x{gh}')

    for name, (legacy, canvas, safe) in DENSITIES.items():
        d = os.path.join(ANDROID_RES, f'mipmap-{name}')
        os.makedirs(d, exist_ok=True)
        to_png(scale_rgba(master, N, legacy), legacy, os.path.join(d, 'ic_launcher.png'))
        to_png(circle_mask(scale_rgba(master, N, legacy), legacy), legacy,
               os.path.join(d, 'ic_launcher_round.png'))
        s = min(safe / gw, safe / gh)
        tw, th = max(1, int(gw * s)), max(1, int(gh * s))
        glyph = crop_scale_rgba(fg, N, minx, miny, gw, gh, tw, th)
        layer = bytearray(canvas * canvas * 4)
        ox, oy = (canvas - tw) // 2, (canvas - th) // 2
        for y in range(th):
            for x in range(tw):
                o = (y * tw + x) * 4
                o2 = ((y + oy) * canvas + (x + ox)) * 4
                layer[o2:o2 + 4] = glyph[o:o + 4]
        to_png(layer, canvas, os.path.join(d, 'ic_launcher_foreground.png'))

    # 自适应图标的底色（颜色资源）
    colors = os.path.join(ANDROID_RES, 'values/colors.xml')
    src = open(colors, encoding='utf-8').read()
    new = re.sub(r'(<color name="ic_launcher_background">)#[0-9A-Fa-f]{6,8}',
                 r'\g<1>#%02X%02X%02X' % mid, src)
    if new != src:
        open(colors, 'w', encoding='utf-8').write(new)
        print('  →', os.path.relpath(colors, ROOT), '#%02X%02X%02X' % mid)


def build_desktop():
    """PC 图标交给 Tauri 官方 icon 子命令（icns/ico 的封装不适合自己写）"""
    cli = None
    if shutil.which('cargo-tauri'):
        cli = ['cargo', 'tauri', 'icon']
    else:
        hits = glob.glob(os.path.expanduser('~/.npm/_npx/*/node_modules/@tauri-apps/cli/tauri.js'))
        if hits:
            cli = ['node', sorted(hits)[0], 'icon']
    if not cli:
        print('  ! 未找到 Tauri CLI，请手动执行：')
        print(f'    npx @tauri-apps/cli icon {os.path.relpath(MASTER, ROOT)}')
        return
    cmd = cli + ['--output', os.path.join(ROOT, 'desktop/src-tauri/icons'), MASTER]
    env = dict(os.environ)
    env['PATH'] = os.path.expanduser('~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin') + ':' + env['PATH']
    p = subprocess.run(cmd, capture_output=True, env=env, cwd=ROOT)
    if p.returncode != 0:
        sys.exit('tauri icon 失败: ' + p.stderr.decode()[:500])
    # 该子命令会顺带产出 Tauri 移动端的图标目录；本项目移动端是 Flutter，用不上
    for extra in ('android', 'ios'):
        shutil.rmtree(os.path.join(ROOT, 'desktop/src-tauri/icons', extra), ignore_errors=True)
    for f in sorted(glob.glob(os.path.join(ROOT, 'desktop/src-tauri/icons/*'))):
        print('  →', os.path.relpath(f, ROOT))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--check', action='store_true', help='只校验母版')
    args = ap.parse_args()

    w, h = png_size(MASTER)
    print(f'母版 {os.path.relpath(MASTER, ROOT)}：{w}×{h}')
    if w != h or w < 512:
        sys.exit('母版必须是 512 以上的正方形')
    if args.check:
        return
    print('PC 图标：')
    build_desktop()
    print('Android 图标：')
    build_android(decode(MASTER, N))
    print('完成。')


if __name__ == '__main__':
    main()
