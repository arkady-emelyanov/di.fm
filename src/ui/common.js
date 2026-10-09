function send(msg) { window.ipc.postMessage(JSON.stringify(msg)); }

function el(tag, attrs, children) {
  var node = document.createElement(tag);
  Object.keys(attrs || {}).forEach(function (k) {
    if (k === 'class') node.className = attrs[k];
    else if (k === 'text') node.textContent = attrs[k];
    else if (k.slice(0, 2) === 'on') node.addEventListener(k.slice(2), attrs[k]);
    else node.setAttribute(k, attrs[k]);
  });
  (children || []).forEach(function (c) { node.appendChild(c); });
  return node;
}

var PLAY_MSG = { channel: 'play', playlist: 'playPlaylist', show: 'playShow' };

// `kind` is 'channel' (default), 'playlist' or 'show'.
function channelTile(ch, kind) {
  kind = kind || 'channel';
  var img = el('img', { alt: '', draggable: 'false', loading: 'lazy' });
  if (ch.image) img.src = ch.image;
  var tile = el('div', { class: 'tile', title: ch.name, 'data-id': ch.id, 'data-kind': kind }, [
    img, el('div', { class: 'eq' }, [el('i'), el('i'), el('i')])
  ]);
  tile.addEventListener('click', function () {
    send({ kind: PLAY_MSG[kind], id: ch.id });
  });
  return tile;
}

function markActive(container, state) {
  var active = state.status !== 'stopped' ? state.mediaId : null;
  container.querySelectorAll('.tile').forEach(function (t) {
    var on = Number(t.getAttribute('data-id')) === active && t.getAttribute('data-kind') === state.mediaKind;
    t.classList.toggle('active', on);
    t.classList.toggle('paused', on && state.status === 'paused');
  });
}

var ICONS = {
  play: '<svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z"/></svg>',
  pause: '<svg viewBox="0 0 24 24"><path d="M6 5h4v14H6zm8 0h4v14h-4z"/></svg>',
  stop: '<svg viewBox="0 0 24 24"><path d="M6 6h12v12H6z"/></svg>',
  channel: '<svg viewBox="0 0 24 24"><path d="M3.24 6.15C2.51 6.43 2 7.17 2 8v12a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V8c0-1.11-.89-2-2-2H8.3l8.26-3.34L15.88 1 3.24 6.15zM7 20a3 3 0 1 1 0-6 3 3 0 0 1 0 6zm13-8h-2v-2h-2v2H4V8h16v4z"/></svg>',
  playlist: '<svg viewBox="0 0 24 24"><path d="M15 6H3v2h12V6zm0 4H3v2h12v-2zM3 16h8v-2H3v2zM17 6v8.18A3 3 0 0 0 16 14a3 3 0 1 0 3 3V8h3V6h-5z"/></svg>',
  show: '<svg viewBox="0 0 24 24"><path d="M12 14a3 3 0 0 0 3-3V5a3 3 0 0 0-6 0v6a3 3 0 0 0 3 3zm5.3-3c0 3-2.54 5.1-5.3 5.1S6.7 14 6.7 11H5c0 3.41 2.72 6.23 6 6.72V21h2v-3.28c3.28-.49 6-3.31 6-6.72h-1.7z"/></svg>',
  skip: '<svg viewBox="0 0 24 24"><path d="M6 18l8.5-6L6 6v12zm10-12v12h2V6h-2z"/></svg>',
  settings: '<svg viewBox="0 0 24 24"><path d="M19.14 12.94a7.07 7.07 0 0 0 0-1.88l2.03-1.58a.5.5 0 0 0 .12-.64l-1.92-3.32a.5.5 0 0 0-.61-.22l-2.39.96a7.03 7.03 0 0 0-1.62-.94l-.36-2.54a.5.5 0 0 0-.5-.42h-3.84a.5.5 0 0 0-.49.42l-.36 2.54a7.36 7.36 0 0 0-1.62.94l-2.39-.96a.5.5 0 0 0-.61.22L2.71 8.84a.5.5 0 0 0 .12.64l2.03 1.58a7.07 7.07 0 0 0 0 1.88l-2.03 1.58a.5.5 0 0 0-.12.64l1.92 3.32c.12.22.38.3.61.22l2.39-.96c.5.38 1.04.7 1.62.94l.36 2.54c.05.24.25.42.49.42h3.84c.25 0 .45-.18.49-.42l.36-2.54a7.36 7.36 0 0 0 1.62-.94l2.39.96c.23.08.49 0 .61-.22l1.92-3.32a.5.5 0 0 0-.12-.64l-2.03-1.58zM12 15.6a3.6 3.6 0 1 1 0-7.2 3.6 3.6 0 0 1 0 7.2z"/></svg>',
  star: '<svg viewBox="0 0 24 24"><path d="M12 17.3 18.2 21l-1.6-7L22 9.2l-7.2-.6L12 2 9.2 8.6 2 9.2 7.4 14l-1.6 7z"/></svg>'
};

// Puts the media kind's icon in front of a tab's label.
function tabIcon(button, kind) {
  button.innerHTML = ICONS[kind] + '<span>' + button.textContent + '</span>';
}

document.addEventListener('contextmenu', function (e) { e.preventDefault(); });
window.addEventListener('DOMContentLoaded', function () { send({ kind: 'ready' }); });
