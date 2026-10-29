// Live sound through Cloudflare Realtime (owner's choice, 2026-10-28): the speaker's page sends
// its microphone to the Realtime SFU over WebRTC, as /dev/snd/NAME; a listener's page receives it
// from there. The authority (api.rs, sound) speaks to Realtime with the app's secret, and decides
// who may: the speaker's account, which records the sound into a file, and whoever may read that
// file.
import { json } from './http.mjs';

const ICE = [{ urls: 'stun:stun.cloudflare.com:3478' }];
// How often a speaker's page says it still sends its sound, in ms.
const HOLD = 30000;

const post = (url, body, method = 'POST') => fetch(url, { method, headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) }).then(json);

// Until `pc` connects, or fails, or ten seconds pass.
function connected(pc) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('The sound did not connect')), 10000);
    const check = () => {
      if (pc.connectionState === 'connected') { clearTimeout(timer); resolve(); }
      else if (['failed', 'closed'].includes(pc.connectionState)) { clearTimeout(timer); reject(new Error('The sound did not connect')); }
    };
    pc.addEventListener('connectionstatechange', check); check();
  });
}

/** Send `microphone` live, as /dev/snd/NAME: of the recording at `path`, or, with no path, kept
 * nowhere, a file of its own made with the ACL entries `acl` (stream -m). Settles with the device
 * and what ends it. While it goes, the page says so (/api/sound/hold), or it ends
 * (devices::SOUND_IDLE). */
export async function speak(path, name, microphone, acl = null) {
  const pc = new RTCPeerConnection({ iceServers: ICE, bundlePolicy: 'max-bundle' });
  let published = null, holding = null;
  const device = () => 'name=' + encodeURIComponent(published.device) + '&version=' + encodeURIComponent(published.version ?? '');
  // Ends the device this page made, while it is still that one: another page of the account may
  // have taken its name over since (devices::stop, by version).
  const end = async () => {
    clearInterval(holding); pc.close();
    if (!published) return;
    await fetch('/api/sound?' + device(), { method: 'DELETE' }).catch(() => {});
  };
  try {
    const transceiver = pc.addTransceiver(microphone.getAudioTracks()[0], { direction: 'sendonly' });
    await pc.setLocalDescription(await pc.createOffer());
    const query = new URLSearchParams();
    if (path) query.set('path', path);
    if (name) query.set('name', name);
    if (acl) query.set('acl', acl);
    published = await post('/api/sound?' + query, { sdp: pc.localDescription.sdp, mid: transceiver.mid });
    await pc.setRemoteDescription({ type: 'answer', sdp: published.sdp });
    await connected(pc);
    // Still sending: every half minute; a device gone or taken over is kept no more.
    holding = setInterval(() => {
      fetch('/api/sound/hold?' + device(), { method: 'POST' }).then(json).catch(error => { if (error.status) clearInterval(holding); });
    }, HOLD);
    return { device: published.device, end };
  } catch (error) {
    // A sound that never connected is no device for listeners to find.
    await end();
    throw error;
  }
}

/** Hear the sound /dev/snd/NAME live in `audio`; settles with what stops it, and what the device
 * is (its version), to know whether it is replaced while the page's line is down. */
export async function listen(device, audio) {
  const pc = new RTCPeerConnection({ iceServers: ICE, bundlePolicy: 'max-bundle' });
  try {
    pc.ontrack = event => { audio.srcObject = new MediaStream([event.track]); };
    const offered = await post('/api/sound/listen?path=' + encodeURIComponent(device), {});
    await pc.setRemoteDescription({ type: 'offer', sdp: offered.sdp });
    await pc.setLocalDescription(await pc.createAnswer());
    await post('/api/sound/answer?session=' + encodeURIComponent(offered.session), { sdp: pc.localDescription.sdp }, 'PUT');
    await connected(pc);
    return { stop: () => { pc.close(); audio.srcObject = null; }, version: offered.version };
  } catch (error) { pc.close(); throw error; }
}
