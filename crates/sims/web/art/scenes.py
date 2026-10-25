"""The pictures of the simulation page: a car at a charging post, a bus under a pantograph (ACDP) and a bus under an
infrastructure arm (OppCharge), made from a small 3D model (faces, a camera, painter's order) and written into
src/index.html between the scenes:begin and scenes:end markers.

    python3 crates/sims/web/art/scenes.py crates/sims/web/src/index.html
"""
import math, re, sys

def sub(a, b): return (a[0]-b[0], a[1]-b[1], a[2]-b[2])
def cross(a, b): return (a[1]*b[2]-a[2]*b[1], a[2]*b[0]-a[0]*b[2], a[0]*b[1]-a[1]*b[0])
def dot(a, b): return a[0]*b[0]+a[1]*b[1]+a[2]*b[2]
def norm(a):
    l = math.sqrt(dot(a, a)) or 1
    return (a[0]/l, a[1]/l, a[2]/l)

class Scene:
    def __init__(s, cam, pitch, f, cx, cy):
        s.cam, s.a, s.f, s.cx, s.cy = cam, math.radians(pitch), f, cx, cy
        s.out = []
        s.light = norm((-0.45, 0.85, 0.5))

    def place(s, P, x, y):
        s.cx = s.cy = 0
        px, py = s.p(P)
        s.cx, s.cy = x-px, y-py

    def p(s, P):
        d = sub(P, s.cam)
        c, sn = math.cos(s.a), math.sin(s.a)
        xc, yc, zc = d[0], d[1]*c - d[2]*sn, -d[1]*sn - d[2]*c
        return (s.cx + s.f*xc/zc, s.cy - s.f*yc/zc)

    def pts(s, P):
        return " ".join("%.1f,%.1f" % s.p(q) for q in P)

    def shade(s, P):
        n = norm(cross(sub(P[1], P[0]), sub(P[2], P[0])))
        if dot(n, sub(s.cam, P[0])) < 0: n = (-n[0], -n[1], -n[2])
        return max(0.0, dot(n, s.light))

    def face(s, P, fill, shaded=True, extra="", opacity=None):
        o = s.pts(P)
        st = "fill:%s" % fill + (";opacity:%s" % opacity if opacity is not None else "")
        s.out.append('<polygon points="%s" style="%s" %s/>' % (o, st, extra))
        if shaded:
            l = s.shade(P)
            if l < 0.5: s.out.append('<polygon points="%s" fill="#000" opacity="%.2f"/>' % (o, (0.5-l)*0.9))
            elif l > 0.7: s.out.append('<polygon points="%s" fill="#fff" opacity="%.2f"/>' % (o, (l-0.7)*0.35))

    def line(s, P, stroke, w=1.5, op=1, extra=""):
        s.out.append('<polyline points="%s" fill="none" style="stroke:%s" stroke-width="%s" stroke-opacity="%s" stroke-linecap="round" stroke-linejoin="round" %s/>' % (s.pts(P), stroke, w, op, extra))

    def raw(s, text): s.out.append(text)

    def box(s, x0, x1, y0, y1, z0, z1, fill, skip=(), T=None):
        v = T if T else (lambda x, y, z: (x, y, z))
        faces = {
            "top": [v(x0,y1,z0), v(x1,y1,z0), v(x1,y1,z1), v(x0,y1,z1)],
            "front": [v(x0,y0,z1), v(x1,y0,z1), v(x1,y1,z1), v(x0,y1,z1)],
            "back": [v(x0,y0,z0), v(x1,y0,z0), v(x1,y1,z0), v(x0,y1,z0)],
            "left": [v(x0,y0,z0), v(x0,y0,z1), v(x0,y1,z1), v(x0,y1,z0)],
            "right": [v(x1,y0,z0), v(x1,y0,z1), v(x1,y1,z1), v(x1,y1,z0)],
        }
        vis = []
        for k, P in faces.items():
            if k in skip: continue
            n = norm(cross(sub(P[1], P[0]), sub(P[2], P[0])))
            c = tuple(sum(q[i] for q in P)/4 for i in range(3))
            if dot(n, sub(s.cam, c)) > 0 or (dot(n, sub(s.cam, c)) < 0 and False):
                vis.append((k, P))
            else:
                n = (-n[0], -n[1], -n[2])
                if dot(n, sub(s.cam, c)) > 0: vis.append((k, P))
        # a box's visible faces never overlap; any order will do
        for k, P in vis: s.face(P, fill)
        return dict(vis)

def circle(center, r, plane, n=40, start=0, end=2*math.pi, clampy=None):
    pts = []
    for i in range(n+1):
        a = start + (end-start)*i/n
        c, sn = math.cos(a)*r, math.sin(a)*r
        if plane == "xy": q = (center[0]+c, center[1]+sn, center[2])
        else: q = (center[0], center[1]+sn, center[2]+c)
        if clampy is not None: q = (q[0], max(q[1], clampy), q[2])
        pts.append(q)
    return pts

class Frame:
    """Local car coordinates (forward, up, right) to the world."""
    def __init__(s, pos, yaw):
        a = math.radians(yaw)
        s.pos, s.F, s.R = pos, (math.cos(a), 0, math.sin(a)), (-math.sin(a), 0, math.cos(a))
    def __call__(s, lx, ly, lz):
        return (s.pos[0]+lx*s.F[0]+lz*s.R[0], s.pos[1]+ly, s.pos[2]+lx*s.F[2]+lz*s.R[2])

def wheel(S, T, lx, r, lz, steer=0):
    c = T(lx, r, lz)
    # the tyre's far face, a wider dark disc, the tyre and the rim
    def disc(rr, z, fill, op=1):
        P = [T(lx+rr*math.cos(a), r+rr*math.sin(a), z) for a in [i*2*math.pi/40 for i in range(40)]]
        S.raw('<polygon points="%s" style="fill:%s" opacity="%s"/>' % (S.pts(P), fill, op))
    disc(r*1.18, lz-0.1, "#07090c")
    disc(r*1.0, lz-0.05, "#0d0f12")
    disc(r*1.0, lz, "#15181c")
    disc(r*0.86, lz+0.005, "#262b31")
    disc(r*0.70, lz+0.01, "url(#rim)")
    disc(r*0.50, lz+0.012, "#3a4047")
    for i in range(5):
        a = 0.35 + i*2*math.pi/5
        e = T(lx+r*0.64*math.cos(a), r+r*0.64*math.sin(a), lz+0.02)
        S.line([T(lx, r, lz+0.02), e], "#e6e9ec", 3.2)
    disc(r*0.14, lz+0.03, "#cfd4d9")
    return S.p(c)

def arch(S, T, lx, r, lz):
    P = [T(lx+(r+0.07)*math.cos(a), max(0.02, r+(r+0.07)*math.sin(a)), lz) for a in [i*math.pi/30 for i in range(0, 31)]]
    S.raw('<polygon points="%s" style="fill:#07090c"/>' % S.pts(P))

DEFS = '''<svg width="0" height="0" style="position:absolute" aria-hidden="true"><defs>
<linearGradient id="flank" x1="0" y1="0" x2="0" y2="1"><stop offset="0" style="stop-color:var(--body-hi)"/><stop offset=".35" style="stop-color:var(--body)"/><stop offset="1" style="stop-color:var(--body2)"/></linearGradient>
<linearGradient id="glassg" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#050a10"/><stop offset=".55" stop-color="#16263a"/><stop offset="1" stop-color="#34506f"/></linearGradient>
<linearGradient id="streak" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="#fff" stop-opacity="0"/><stop offset=".5" stop-color="#fff" stop-opacity=".2"/><stop offset="1" stop-color="#fff" stop-opacity="0"/></linearGradient>
<radialGradient id="rim" cx=".4" cy=".35" r=".8"><stop offset="0" stop-color="#f4f6f8"/><stop offset=".6" stop-color="#9aa2ab"/><stop offset="1" stop-color="#555d66"/></radialGradient>
<radialGradient id="glow" cx=".5" cy=".5" r=".5"><stop offset="0" stop-color="#fff7cf" stop-opacity=".9"/><stop offset="1" stop-color="#fff7cf" stop-opacity="0"/></radialGradient>
<linearGradient id="skyg" x1="0" y1="0" x2="0" y2="1"><stop offset="0" style="stop-color:var(--sky2)"/><stop offset="1" style="stop-color:var(--sky)"/></linearGradient>
<linearGradient id="groundg" x1="0" y1="0" x2="0" y2="1"><stop offset="0" style="stop-color:var(--ground2)"/><stop offset="1" style="stop-color:var(--ground)"/></linearGradient>
<filter id="soft" x="-20%" y="-200%" width="140%" height="500%"><feGaussianBlur stdDeviation="5"/></filter>
</defs></svg>'''

def backdrop(S, W, H):
    horizon = S.p((0, 0, -400))[1]
    S.raw('<rect x="0" y="0" width="%d" height="%d" fill="url(#skyg)"/>' % (W, H))
    S.raw('<rect x="0" y="%.1f" width="%d" height="%d" fill="url(#groundg)"/>' % (horizon, W, H))
    S.raw('<line x1="0" x2="%d" y1="%.1f" y2="%.1f" style="stroke:var(--ground2)" stroke-width="1"/>' % (W, horizon, horizon))

def bay(S, T, x0, x1, z0, z1):
    for a, b in (((x0, z0), (x0, z1)), ((x1, z0), (x1, z1)), ((x0, z1), (x1, z1))):
        S.line([T(a[0], 0, a[1]), T(b[0], 0, b[1])], "#fff", 2.5, .55)

def shadow(S, T, x0, x1, z0, z1, op=.5):
    P = [T(x0, 0, z0), T(x1, 0, z0), T(x1, 0, z1), T(x0, 0, z1)]
    S.raw('<polygon points="%s" fill="#000" opacity="%s" filter="url(#soft)"/>' % (S.pts(P), op))

def post(S, x, z, h=1.7):
    """The charging post: a slim cabinet with a screen and a light strip. Returns the cable holster on its face."""
    S.box(x-0.28, x+0.28, 0, h, z-0.2, z+0.2, "#d9dee4")
    # face toward the camera (+z): a dark screen and a coloured strip
    S.face([(x-0.2, h-0.55, z+0.201), (x+0.2, h-0.55, z+0.201), (x+0.2, h-0.2, z+0.201), (x-0.2, h-0.2, z+0.201)], "#12171d", False)
    S.face([(x-0.2, h-0.16, z+0.201), (x+0.2, h-0.16, z+0.201), (x+0.2, h-0.1, z+0.201), (x-0.2, h-0.1, z+0.201)], "var(--live)", False)
    S.box(x-0.3, x+0.3, h, h+0.08, z-0.22, z+0.22, "#9aa3ad")
    return (x, h-0.75, z+0.21)

def car_scene(W=800, H=320):
    S = Scene(cam=(-0.2, 3.3, 10.5), pitch=14, f=900, cx=0, cy=0)
    T = Frame((0, 0, 0), 27)
    S.place(T(0, 0, 0), 330, 222)
    backdrop(S, W, H)
    bay(S, T, -2.8, 3.4, -1.55, 1.9)
    shadow(S, T, -2.3, 2.4, -1.0, 1.0, .55)
    S.out.append('<g class="veh">')
    K = "lx"
    Wd = 0.9
    # top surfaces
    S.face([T(0.1, 1.45, 0.7), T(0.1, 1.45, -0.7), T(-1.0, 1.44, -0.7), T(-1.0, 1.44, 0.7)], "var(--body-hi)")
    S.face([T(-1.0, 1.44, 0.7), T(-1.0, 1.44, -0.7), T(-1.95, 1.02, -0.8), T(-1.95, 1.02, 0.8)], "url(#glassg)", False)
    S.face([T(-1.95, 1.02, 0.8), T(-1.95, 1.02, -0.8), T(-2.15, 0.98, -0.85), T(-2.15, 0.98, 0.85)], "var(--body)")
    S.face([T(0.95, 1.0, 0.82), T(0.95, 1.0, -0.82), T(0.1, 1.45, -0.7), T(0.1, 1.45, 0.7)], "url(#glassg)", False)
    S.face([T(0.7, 1.12, 0.6), T(0.55, 1.2, 0.0), T(0.4, 1.3, 0.55), T(0.2, 1.4, 0.2)], "#fff", False, opacity=0)
    S.face([T(0.95, 1.0, 0.86), T(2.15, 0.72, 0.86), T(2.15, 0.72, -0.86), T(0.95, 1.0, -0.86)], "var(--body-hi)")
    # hood centre line and the windshield streak
    S.line([T(1.0, 1.0, 0.0), T(2.15, 0.72, 0.0)], "#000", 1, .18)
    S.raw('<polygon points="%s" fill="url(#streak)"/>' % S.pts([T(0.8, 1.02, 0.55), T(0.2, 1.43, 0.55), T(0.1, 1.43, 0.25), T(0.7, 1.02, 0.25)]))
    # greenhouse wall (near) and glass
    S.face([T(0.95, 1.0, 0.8), T(0.1, 1.45, 0.7), T(-1.0, 1.44, 0.7), T(-1.95, 1.02, 0.8)], "var(--body)")
    # front
    S.face([T(2.15, 0.72, 0.86), T(2.15, 0.72, -0.86), T(2.2, 0.45, -0.84), T(2.2, 0.45, 0.84)], "var(--body)")
    S.face([T(2.2, 0.45, 0.84), T(2.2, 0.45, -0.84), T(2.12, 0.27, -0.8), T(2.12, 0.27, 0.8)], "#14181d", False)
    S.face([T(2.201, 0.56, 0.4), T(2.201, 0.56, -0.4), T(2.201, 0.46, -0.4), T(2.201, 0.46, 0.4)], "#0a0d11", False)
    for sg in (1, -1):
        hl = [T(2.16, 0.7, sg*0.84), T(2.16, 0.7, sg*0.46), T(2.17, 0.6, sg*0.5), T(2.17, 0.58, sg*0.82)]
        S.face(hl, "#fff6c8", False)
        cx_, cy_ = S.p(T(2.17, 0.64, sg*0.66))
        S.raw('<circle cx="%.1f" cy="%.1f" r="14" fill="url(#glow)"/>' % (cx_, cy_))
    # flank
    flank = [(-2.15, 0.28), (-2.2, 0.62), (-2.12, 0.98), (-1.0, 1.0), (0.95, 1.0), (1.9, 0.86), (2.2, 0.72), (2.2, 0.42), (2.12, 0.28)]
    S.raw('<polygon points="%s" style="fill:url(#flank)"/>' % S.pts([T(x, y, Wd) for x, y in flank]))
    # character lines: shoulder light, waist shadow, accent
    S.line([T(-2.1, 0.93, Wd), T(0.9, 0.95, Wd), T(1.9, 0.82, Wd)], "#fff", 2.2, .55)
    S.line([T(-2.12, 0.7, Wd), T(-0.4, 0.62, Wd), T(1.4, 0.64, Wd), T(2.15, 0.6, Wd)], "#000", 1.6, .25)
    S.line([T(-2.12, 0.5, Wd), T(2.15, 0.5, Wd)], "var(--live)", 2.2, .9)
    for x in (-0.12, 1.02):
        S.line([T(x, 0.97, Wd), T(x, 0.3, Wd)], "#000", 1.3, .35)
    S.line([T(1.0, 0.88, Wd), T(1.0, 0.3, Wd), T(1.0, 0.3, Wd)], "#000", 1.3, .0)
    S.line([T(0.12, 0.88, Wd+.005), T(0.38, 0.88, Wd+.005)], "#000", 3, .4)
    S.line([T(0.12, 0.9, Wd+.005), T(0.38, 0.9, Wd+.005)], "#fff", 1, .55)
    # side glass
    for g in ([(0.8, 1.04), (0.12, 1.4), (-0.15, 1.4), (-0.13, 1.04)], [(-0.31, 1.04), (-0.3, 1.4), (-0.98, 1.4), (-1.72, 1.04)]):
        P = [T(x, y, 0.8) for x, y in g]
        S.raw('<polygon points="%s" style="fill:url(#glassg)"/>' % S.pts(P))
        S.raw('<polygon points="%s" fill="url(#streak)"/>' % S.pts([T(g[0][0]-0.1, 1.04, 0.8), T(g[1][0]-0.05, 1.4, 0.8), T(g[1][0]-0.2, 1.4, 0.8), T(g[0][0]-0.35, 1.04, 0.8)]))
        S.line(P + [P[0]], "#07090c", 2.5, .9)
    S.line([T(-2.12, 0.98, Wd), T(-1.95, 1.02, 0.8)], "#fff", 1.5, .4)
    # tail light
    S.face([T(-2.2, 0.82, 0.88), T(-2.2, 0.72, 0.88), T(-2.17, 0.74, 0.5), T(-2.17, 0.84, 0.5)], "#c9302a", False)
    # charge port (front fender)
    port = [T(1.62, 0.86, Wd+.004), T(1.84, 0.86, Wd+.004), T(1.84, 0.7, Wd+.004), T(1.62, 0.7, Wd+.004)]
    S.raw('<polygon points="%s" style="fill:#0d1116"/>' % S.pts(port))
    px, py = S.p(T(1.73, 0.78, Wd+.01))
    S.raw('<circle cx="%.1f" cy="%.1f" r="2.6" style="fill:var(--live)"/><circle cx="%.1f" cy="%.1f" r="7" style="fill:var(--live)" opacity=".25"/>' % (px, py, px, py))
    # wheels (near side)
    for lx in (1.4, -1.4):
        arch(S, T, lx, 0.34, Wd+.002)
    for lx in (1.4, -1.4):
        wheel(S, T, lx, 0.34, Wd-0.02)
    S.out.append('</g>')
    # the post and the cable
    holster = post(S, *(lambda q: (q[0], q[2]))(T(3.1, 0, 1.25)))
    hx, hy = S.p(holster)
    S.out.append('<g class="cablegrp"><path id="cable" class="draw" d="M%.1f,%.1f Q%.1f,%.1f %.1f,%.1f" pathLength="100"/><path class="flow" d="M%.1f,%.1f Q%.1f,%.1f %.1f,%.1f"/></g>' % (
        hx, hy, (hx+px)/2, max(hy, py)+38, px, py, hx, hy, (hx+px)/2, max(hy, py)+38, px, py))
    d0 = S.p(T(0, 0, 0)); d1 = S.p(T(-9, 0, 0)); d2 = S.p(T(9, 0, 0))
    return S, (d1[0]-d0[0], d1[1]-d0[1], d2[0]-d0[0], d2[1]-d0[1])

def bus_scene(kind, W=800, H=320):
    S = Scene(cam=(-0.5, 4.4, 24), pitch=11, f=950, cx=0, cy=0)
    T = Frame((0, 0, 0), 24)
    S.place(T(0, 0, 0), 390, 262)
    backdrop(S, W, H)
    bay(S, T, -5.9, 6.0, -1.9, 2.6)
    # the mast stands behind the bus (in the bus's own frame), its arm reaches over the roof centre line
    ay = 4.9
    MX, MZ = 0.6, -3.1
    S.box(MX-0.2, MX+0.2, 0, ay, MZ-0.2, MZ+0.2, "#d9dee4", T=T)
    S.box(MX-0.2, MX+0.2, ay-0.2, ay+0.1, MZ-0.2, 0.45, "#9aa3ad", T=T)
    S.box(MX-0.3, MX+0.3, ay-0.3, ay+0.2, MZ-0.3, MZ+0.3, "#9aa3ad", T=T)
    shadow(S, T, -5.3, 5.3, -1.2, 1.2, .55)
    if kind == "acdp":
        S.box(MX-0.7, MX+0.7, ay-0.44, ay-0.2, -0.4, 0.45, "#6c757f", T=T)
    mx, mz = T(MX, 0, 0)[0], T(MX, 0, 0)[2]
    S.out.append('<g class="veh">')
    BH, HW, L = 3.25, 1.27, 5.1
    # roof and its rounded front edge
    S.face([T(-L, BH, HW), T(L-0.3, BH, HW), T(L-0.3, BH, -HW), T(-L, BH, -HW)], "var(--body-hi)")
    S.face([T(L-0.3, BH, HW-0.1), T(L, 3.05, HW-0.1), T(L, 3.05, -HW+0.1), T(L-0.3, BH, -HW+0.1)], "var(--body-hi)")
    S.face([T(-L+0.7, BH+0.18, 0.9), T(L-1.6, BH+0.18, 0.9), T(L-1.6, BH+0.18, -0.9), T(-L+0.7, BH+0.18, -0.9)], "var(--body2)")
    S.face([T(-L+0.7, BH, 0.9), T(L-1.6, BH, 0.9), T(L-1.6, BH+0.18, 0.9), T(-L+0.7, BH+0.18, 0.9)], "var(--body2)")
    # front
    S.face([T(L, 3.05, HW-0.1), T(L, 3.05, -HW+0.1), T(L+0.0, 0.62, -HW+0.1), T(L, 0.62, HW-0.1)], "var(--body)")
    S.face([T(L+0.002, 2.9, HW-0.3), T(L+0.002, 2.9, -HW+0.3), T(L+0.002, 1.2, -HW+0.3), T(L+0.002, 1.2, HW-0.3)], "url(#glassg)", False)
    S.raw('<polygon points="%s" fill="url(#streak)"/>' % S.pts([T(L+0.004, 2.9, 0.75), T(L+0.004, 1.2, 0.2), T(L+0.004, 1.2, -0.1), T(L+0.004, 2.9, 0.45)]))
    S.face([T(L+0.003, 3.0, 0.8), T(L+0.003, 3.0, -0.8), T(L+0.003, 2.92, -0.8), T(L+0.003, 2.92, 0.8)], "#1c1400", False)
    S.face([T(L+0.004, 2.99, 0.7), T(L+0.004, 2.99, -0.3), T(L+0.004, 2.94, -0.3), T(L+0.004, 2.94, 0.7)], "var(--live)", False)
    S.face([T(L+0.003, 0.9, HW-0.2), T(L+0.003, 0.9, HW-0.55), T(L+0.003, 0.74, HW-0.55), T(L+0.003, 0.74, HW-0.2)], "#fff6c8", False)
    S.face([T(L+0.003, 0.9, -HW+0.2), T(L+0.003, 0.9, -HW+0.55), T(L+0.003, 0.74, -HW+0.55), T(L+0.003, 0.74, -HW+0.2)], "#fff6c8", False)
    # flank
    fl = [T(-L, 0.45, HW), T(L, 0.62, HW-0.1), T(L, 3.05, HW-0.1), T(L-0.3, BH, HW), T(-L, BH, HW)]
    S.raw('<polygon points="%s" style="fill:url(#flank)"/>' % S.pts(fl))
    S.face([T(-L, 0.45, HW), T(L, 0.62, HW-0.1), T(L, 0.62, HW-0.1), T(-L, 0.45, HW)], "#000", False, opacity=0)
    # lower accent swoosh
    sw = [T(-L, 0.85, HW+.003), T(-1.5, 1.0, HW+.003), T(2, 1.28, HW+.003), T(L, 1.35, HW+.003), T(L, 1.12, HW+.003), T(2, 1.02, HW+.003), T(-1.5, 0.72, HW+.003), T(-L, 0.62, HW+.003)]
    S.raw('<polygon points="%s" style="fill:var(--live)"/>' % S.pts(sw))
    S.line([T(-L, 0.9, HW+.004), T(-1.5, 1.04, HW+.004), T(2, 1.31, HW+.004), T(L, 1.38, HW+.004)], "#fff", 1.3, .55)
    # windows
    wz = HW + .004
    for i in range(6):
        x0 = -4.7 + i*1.12
        P = [T(x0, 2.75, wz), T(x0+0.98, 2.75, wz), T(x0+0.98, 1.5, wz), T(x0, 1.5, wz)]
        S.raw('<polygon points="%s" style="fill:url(#glassg)"/>' % S.pts(P))
        S.raw('<polygon points="%s" fill="url(#streak)"/>' % S.pts([T(x0+0.1, 1.5, wz), T(x0+0.5, 2.75, wz), T(x0+0.75, 2.75, wz), T(x0+0.35, 1.5, wz)]))
        S.line(P + [P[0]], "#07090c", 2, .85)
    # front double door
    for k in range(2):
        x0 = 3.0 + k*0.55
        P = [T(x0, 2.7, wz), T(x0+0.5, 2.7, wz), T(x0+0.5, 0.78, wz), T(x0, 0.78, wz)]
        S.raw('<polygon points="%s" style="fill:url(#glassg)" opacity=".95"/>' % S.pts(P))
        S.line(P + [P[0]], "#07090c", 2, .85)
    S.line([T(-L, 3.0, HW), T(L-0.3, 3.0, HW)], "#fff", 1.8, .5)
    S.line([T(-L, 0.45, HW), T(L, 0.62, HW-0.1)], "#000", 2, .5)
    # panel seam and roof kit
    # wheels
    for lx in (3.55, -2.9):
        arch(S, T, lx, 0.5, HW+.002)
    for lx in (3.55, -2.9):
        wheel(S, T, lx, 0.5, HW-0.02)
    S.out.append('</g>')
    # connection gear on the roof
    if kind == "acdp":
        base = T(0.6, BH+0.18, 0.0)
        gear(S, T, "acdp", mx, mz, ay)
    else:
        gear(S, T, "opp", mx, mz, ay)
    d0 = S.p(T(0, 0, 0)); d1 = S.p(T(-14, 0, 0)); d2 = S.p(T(14, 0, 0))
    return S, (d1[0]-d0[0], d1[1]-d0[1], d2[0]-d0[0], d2[1]-d0[1])

def gear(S, T, kind, mx, mz, ay):
    BH = 3.25
    X = 0.6
    if kind == "acdp":
        # the vehicle's pantograph on the roof rises to the contact rail under the arm
        top = ay - 0.46
        base = BH + 0.3
        S.box(X-0.5, X+0.5, BH+0.18, base, -0.45, 0.45, "#59626c", T=T)
        S.raw('<g class="raise" style="transform-origin:%.1fpx %.1fpx">' % S.p(T(X, base, 0)))
        for sg in (1, -1):
            S.line([T(X-0.4*sg, base, 0.25), T(X+0.3*sg, top-0.05, 0.25)], "#59626c", 4)
            S.line([T(X-0.4*sg, base, -0.25), T(X+0.3*sg, top-0.05, -0.25)], "#59626c", 4)
        S.box(X-0.55, X+0.55, top-0.08, top, -0.4, 0.4, "#c9ced4", T=T)
        S.raw('</g>')
        a, b = S.p(T(X, top, 0)), S.p(T(X, base, 0))
        S.raw('<path class="flow" d="M%.1f,%.1f L%.1f,%.1f"/>' % (*a, *b))
    else:
        # the charger's carriage runs down from the arm onto the rails on the roof
        for off in (-0.3, 0.3):
            S.box(X-1.0, X+1.0, BH+0.18, BH+0.26, off-0.06, off+0.06, "#c9ced4", T=T)
        S.raw('<g class="lower" style="transform-origin:%.1fpx %.1fpx">' % S.p(T(X, ay-0.2, 0)))
        S.line([T(X, ay-0.2, 0), T(X, BH+0.34, 0)], "#59626c", 5)
        S.box(X-0.45, X+0.45, BH+0.26, BH+0.36, -0.4, 0.4, "#59626c", T=T)
        S.raw('</g>')
        a, b = S.p(T(X, BH+0.36, 0)), S.p(T(X, BH+0.24, 0))
        S.raw('<path class="flow" d="M%.1f,%.1f L%.1f,%.1f"/>' % (*a, *b))

def svg(S, W, H, off):
    ax, ay, bx, by = off
    return ('<svg class="scene3d" viewBox="0 0 %d %d" role="img" aria-hidden="true" style="--ax:%.0fpx;--ay:%.0fpx">' % (W, H, ax, ay)
            + "".join(S.out) + "</svg>")

def main(page):
    import json
    W, H = 800, 320
    parts = {}
    S, off = car_scene(W, H); parts["plug"] = svg(S, W, H, off)
    S, off = bus_scene("acdp", W, H); parts["acdp"] = svg(S, W, H, off)
    S, off = bus_scene("opp", W, H); parts["opp"] = svg(S, W, H, off)
    block = ("// scenes:begin (generated by crates/sims/web/art/scenes.py from a small 3D model; do not edit by hand)\n"
             "const SCENE_DEFS = %s;\nconst SCENES = %s;\n// scenes:end" % (json.dumps(DEFS), json.dumps(parts)))
    text = open(page).read()
    new = re.sub(r"// scenes:begin.*?// scenes:end", lambda m: block, text, count=1, flags=re.S)
    assert new != text or block in text, "no scenes:begin/scenes:end markers in " + page
    open(page, "w").write(new)

if __name__ == "__main__":
    main(sys.argv[1])
