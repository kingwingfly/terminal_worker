// Live sound through Cloudflare Realtime (owner's choice, 2026-10-28): the speaker's page sends
// its microphone to the Realtime SFU over WebRTC, as /dev/snd/NAME; a listener's page receives it
// from there. The authority (api.rs, sound) speaks to Realtime with the app's secret, and decides
// who may: the speaker's account, which records the sound into a file, and whoever may read that
// file.
const ICE = [{ urls: 'stun:stun.cloudflare.com:3478' }];

async function json(response) {
  let body = {}; try { body = await response.json(); } catch {}
  if (!response.ok) { const error = new Error(body.error || `Request failed (${response.status})`); error.status = response.status; throw error; }
  return body;
}
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

/** Send `microphone` live, as /dev/snd/NAME of the recording at `path`; settles with the device
 * and what ends it. */
export async function speak(path, name, microphone) {
  const pc = new RTCPeerConnection({ iceServers: ICE, bundlePolicy: 'max-bundle' });
  try {
    const transceiver = pc.addTransceiver(microphone.getAudioTracks()[0], { direction: 'sendonly' });
    await pc.setLocalDescription(await pc.createOffer());
    const query = 'path=' + encodeURIComponent(path) + (name ? '&name=' + encodeURIComponent(name) : '');
    const published = await post('/api/sound?' + query, { sdp: pc.localDescription.sdp, mid: transceiver.mid });
    await pc.setRemoteDescription({ type: 'answer', sdp: published.sdp });
    await connected(pc);
    const end = async () => {
      pc.close();
      await fetch('/api/sound?name=' + encodeURIComponent(published.device), { method: 'DELETE' }).catch(() => {});
    };
    return { device: published.device, end };
  } catch (error) { pc.close(); throw error; }
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
