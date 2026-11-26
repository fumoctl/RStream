// Veylo on a TV (Android without a touchscreen): the remote drives focus, and focus is the light.
//  - The arrow keys move to the nearest control in that direction; OK presses it; Back goes back.
//  - What has focus lights the room: its artwork glows behind everything (the ambient layer), drawn
//    small on a canvas and scaled up, so a modest TV GPU never runs a full-screen blur.
//  - Focus is kept: Back from a title lands on its poster again, a title opens on Play.
// Phones and tablets never run this: html.tv scopes every TV style (styles/tv.css).
(() => {
    if (navigator.maxTouchPoints > 0 || document.documentElement.classList.contains('tv')) return;
    document.documentElement.classList.add('tv');
    const DIRS = { ArrowUp: [0, -1], ArrowDown: [0, 1], ArrowLeft: [-1, 0], ArrowRight: [1, 0] };
    const FOCUSABLE = 'button, a[href], input, select, textarea, [tabindex]';
    const visible = el => {
        if (typeof el.checkVisibility === 'function') return el.checkVisibility({ visibilityProperty: true });
        for (let cur = el; cur && cur.nodeType === 1; cur = cur.parentElement) {
            const st = getComputedStyle(cur);
            if (st.display === 'none' || st.visibility === 'hidden' || st.visibility === 'collapse') return false;
        }
        return true;
    };
    const usable = el => !el.disabled && el.tabIndex >= 0 && !el.closest('[inert]')
        && visible(el) && (r => r.width > 0 && r.height > 0)(el.getBoundingClientRect());
    // An open dialog keeps focus inside it.
    const scope = () => [...document.querySelectorAll('[aria-modal="true"]')].filter(visible).pop() || document;
    const typing = el => el.isContentEditable || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT'
        || (el.tagName === 'INPUT' && !['button', 'checkbox', 'radio', 'submit', 'reset'].includes(el.type));
    const gap = (a0, a1, b0, b1) => Math.max(0, b0 - a1, a0 - b1);
    const idle = () => { const a = document.activeElement; return !a || a === document.body || a.classList.contains('app'); };

    // ------------------------------------------------------------ where focus was
    // Remembered for each place (a tab, a title, search). The page is drawn anew on the way back from
    // a title, so a control is found again by its name (and which of the controls with that name).
    const seen = new Map();
    const place = () => {
        const title = document.querySelector('.title-page');
        return document.querySelector('.search') ? 'search' : title ? 'title:' + title.getAttribute('aria-label')
            : 'tab:' + ((document.querySelector('.dock-tab.on') || {}).textContent || '');
    };
    const name = el => el.getAttribute('aria-label') || el.textContent.trim();
    const named = label => [...document.querySelectorAll(FOCUSABLE)].filter(el => name(el) === label);
    document.addEventListener('focusin', e => {
        if (e.target === document.body || e.target.classList.contains('app')) return;
        const label = name(e.target);
        seen.set(place(), label ? { label, nth: named(label).indexOf(e.target) } : null);
        light();
    });
    const remembered = () => seen.get(place());
    const restore = () => {
        const last = remembered();
        if (!last) return null;
        const el = named(last.label)[last.nth];
        return el && usable(el) && scope().contains(el) ? el : null;
    };
    // Arriving somewhere puts focus in view at once: a page still filling in would cut a glide short.
    const land = (el, block = 'center') => {
        el.focus({ preventScroll: true });
        el.scrollIntoView({ block, inline: 'nearest' });
    };

    // ------------------------------------------------------------ the remote
    // Capture phase, ahead of the app's own keys: on a TV ← and → move between controls (on Picks too,
    // where a keyboard's arrows vote; the remote votes with OK on Keep). The player and the videos
    // keep their keys.
    window.addEventListener('keydown', e => {
        const dir = DIRS[e.key];
        if (!dir || !e.isTrusted || e.altKey || e.ctrlKey || e.metaKey) return;
        if (document.querySelector('.app.playing')) return;
        if (dir[0] && document.querySelector('.theatre')) return;
        const active = document.activeElement;
        const inside = !idle() && usable(active);
        // In a text field ← and → move the caret; ↑ and ↓ leave it.
        if (inside && typing(active) && dir[0]) return;
        e.preventDefault();
        e.stopPropagation();
        const all = [...scope().querySelectorAll(FOCUSABLE)].filter(el => el !== active && usable(el));
        let best;
        if (!inside) best = restore() || document.querySelector('.dock-tab.on') || all[0];
        else {
            const a = active.getBoundingClientRect();
            // Within the page first: the top bar floats over it, so it is only reached past the page's top.
            const page = active.closest('.page');
            best = (page && pick(a, dir, active, all.filter(el => page.contains(el)))) || pick(a, dir, active, all);
            // Coming up into the tabs lands on the one that is open.
            if (best && dir[1] && best.classList.contains('dock-tab') && !active.classList.contains('dock-tab')) best = document.querySelector('.dock-tab.on') || best;
        }
        if (!best) return;
        best.focus({ preventScroll: true });
        // Rows scroll to keep the focused one mid-screen; a held key jumps rather than queueing glides.
        best.scrollIntoView({ block: dir[1] || !inside ? 'center' : 'nearest', inline: 'nearest', behavior: e.repeat ? 'auto' : 'smooth' });
    }, true);

    // The nearest control in that direction: in line with this one first, then the nearest, then the
    // most aligned. ← and → stay on their row: at its end they stop rather than jump to another.
    function pick(a, dir, active, all) {
        let best = null, score = Infinity;
        for (const el of all) {
            if (active.contains(el) || el.contains(active)) continue;
            const c = el.getBoundingClientRect();
            const ac = [(a.left + a.right) / 2, (a.top + a.bottom) / 2], cc = [(c.left + c.right) / 2, (c.top + c.bottom) / 2];
            let along, across, centre;
            if (dir[1]) {
                if (dir[1] * (cc[1] - ac[1]) <= 0) continue;
                along = dir[1] > 0 ? c.top - a.bottom : a.top - c.bottom;
                across = gap(a.left, a.right, c.left, c.right);
                centre = Math.abs(cc[0] - ac[0]);
            } else {
                if (dir[0] * (cc[0] - ac[0]) <= 0) continue;
                along = dir[0] > 0 ? c.left - a.right : a.left - c.right;
                across = gap(a.top, a.bottom, c.top, c.bottom);
                if (across > 0) continue;
                centre = Math.abs(cc[1] - ac[1]);
            }
            const s = Math.max(0, along) + across * 4 + centre * 0.05;
            if (s < score) { score = s; best = el; }
        }
        return best;
    }

    // ------------------------------------------------------------ Back at the top
    // Back never leaves the app on a TV (main.rs): on another tab it goes to Home, and on Home it
    // returns to the top of the page and its tabs.
    window.addEventListener('keydown', e => {
        if (e.key !== 'Escape') return;
        // A Picks menu (mood, filters) closes with its own Done, and Back stops there.
        const menu = document.querySelector('.popover');
        const done = menu && menu.querySelector('.pill.accent');
        if (done && !document.querySelector('.app.playing')) {
            e.preventDefault(); e.stopPropagation(); done.click();
            const opener = document.querySelector('.chip-btn.on') || document.querySelector('.duel-bar .chip-btn');
            if (opener) opener.focus({ preventScroll: true });
            return;
        }
        const open = document.querySelector('.title-page, .search, .theatre, .popover, [aria-modal="true"], .app.playing');
        const home = document.querySelector('.dock-tab.on');
        if (open || !home || home !== document.querySelector('.dock-tab')) return;
        const page = document.querySelector('.page');
        if (page) page.scrollTo({ top: 0, behavior: 'smooth' });
        home.focus({ preventScroll: true });
    }, true);

    // ------------------------------------------------------------ focus after the page changes
    // A title opens on Play; Back lands on the poster it came from; a new tab starts on its first
    // control. Checked at most once a frame, only while nothing has focus.
    // A page that is still arriving gets a moment for the poster to come back before focus settles
    // on its first control instead.
    let queued = false, waiting = 0;
    const settle = () => {
        queued = false;
        if (!idle() || document.querySelector('.app.playing')) { waiting = 0; return; }
        const title = document.querySelector('.title-page');
        const button = title && title.querySelector('.play-big, .title-actions .pill.accent');
        const play = button && !button.disabled ? button : null;
        // A new place starts on its first artwork on screen, else its first control on screen, else the tabs.
        const onScreen = sel => [...scope().querySelectorAll(sel)].find(el => usable(el) && el.getBoundingClientRect().top < innerHeight);
        const first = () => onScreen('.page .card, .page .card-art') || onScreen('.page ' + FOCUSABLE.split(', ').join(', .page '))
            || document.querySelector('.dock-tab.on');
        const back = restore();
        // Still arriving: the poster to return to, the rows, or the title's Play (busy finding sources).
        const arriving = remembered() || (title ? button && button.disabled : document.querySelector('.skeleton-card'));
        if (!back && !play && arriving) {
            if (!waiting) { waiting = performance.now(); setTimeout(settle, 2600); }
            if (performance.now() - waiting < 2500) return;
        }
        waiting = 0;
        // Back where you were (from a title, or from the player to its episode), else a title's Play.
        const el = back || play || first();
        // Only a return scrolls mid-screen; anything else opens at the top.
        if (el) land(el, back ? 'center' : 'nearest');
    };
    new MutationObserver(() => { if (!queued) { queued = true; requestAnimationFrame(settle); } })
        .observe(document.body, { childList: true, subtree: true });

    // ------------------------------------------------------------ the light
    // Two small canvases take turns: the new artwork is drawn into the hidden one (shrunk, so it is
    // already soft), then it fades in over the other. The GPU only scales a tiny texture.
    const room = document.createElement('div');
    room.className = 'tv-light';
    room.setAttribute('aria-hidden', 'true');
    const panes = [0, 1].map(() => { const c = document.createElement('canvas'); c.width = 96; c.height = 54; room.append(c); return c; });
    document.body.prepend(room);
    let shown = 0, lit = '';
    const seed = document.createElement('canvas'); seed.width = 12; seed.height = 7;
    function light() {
        const a = document.activeElement;
        const own = a && a.closest && a.closest('.card, .card-art, .lib-item, .week-card, .poster, .duel-card, .person');
        const img = (own && own.querySelector('img')) || document.querySelector('.ambient img');
        if (!img || !img.currentSrc || img.currentSrc === lit) return;
        if (!img.complete || !img.naturalWidth) { img.addEventListener('load', light, { once: true }); return; }
        lit = img.currentSrc;
        // Shrink twice (12×7, then 96×54 with smoothing): a soft field of the artwork's colours.
        const s = seed.getContext('2d');
        s.filter = 'saturate(1.6) brightness(.5)';
        s.drawImage(img, 0, 0, seed.width, seed.height);
        const next = panes[1 - shown], g = next.getContext('2d');
        g.imageSmoothingQuality = 'high';
        g.drawImage(seed, 0, 0, next.width, next.height);
        next.classList.add('on'); panes[shown].classList.remove('on');
        shown = 1 - shown;
    }
    // The opened title's backdrop lights the room when nothing with artwork has focus.
    new MutationObserver(() => { if (!document.activeElement || !document.activeElement.closest('.card, .card-art, .duel-card, .person')) light(); })
        .observe(document.querySelector('.ambient') || document.body, { childList: true, subtree: true, attributes: true, attributeFilter: ['src'] });
    light();
})();
