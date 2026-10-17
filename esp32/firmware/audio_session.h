#pragma once
#include <ESP_I2S.h>
#include "audio_crypto.h"
#include "audio_vectors.h"

// Only loop publishes. Worker never reads mutable Strings/control objects.
struct AudioSnapshot {
  uint32_t generation = 0, epoch = 0, edgeUs = 0, permissionAt = 0, ackAt = 0;
  bool gate = false, valid = false;
  uint8_t material[44] = {}, device[6] = {}, ip[4] = {};
};
struct AudioMetrics {
  uint32_t sent=0, sendErrors=0, readErrors=0, discarded=0, cryptoErrors=0;
  uint32_t firstFrameUs=0, maxFrameUs=0;
  bool exhausted=false, ready=false;
  uint32_t initErrors=0;
};
portMUX_TYPE audioMux = portMUX_INITIALIZER_UNLOCKED;
AudioSnapshot audioShared;
AudioMetrics audioMetrics;
bool audioStarted = false, audioCryptoPassed = false;
uint32_t audioEpoch = 0, audioEdgeUs = 0;
String audioSession;

bool hexDecode(const String &s, uint8_t *out, size_t len) {
  if (s.length() != len*2) return false;
  for (size_t i=0; i<len; ++i) {
    char b[3] = {s[2*i],s[2*i+1],0}; char *end;
    out[i] = strtoul(b,&end,16); if (*end) return false;
  }
  return true;
}

bool audioSelfTest() {
  uint8_t key[32], context[22], material[44], pcm[640], packet[P8::PACKET];
  for (size_t i=0; i<32; ++i) key[i]=i;
  for (size_t i=0; i<640; ++i) pcm[i]=uint8_t(i);
  if (!hexDecode("AABBCCDDEEFF00112233445566778899aabbccddeeff",context,22)) return false;
  return P8::derive(key,context,material) && !memcmp(material,VECTOR_MATERIAL,44) &&
      P8::encrypt(material,context,0,1,pcm,packet) && !memcmp(packet,VECTOR_PACKET,P8::PACKET);
}

void publishAudio() {
  const bool valid = authenticated && versionOK && record.state==PAIRED && !bleOn && WiFi.status()==WL_CONNECTED && audioCryptoPassed;
  const String session = dn + hn;
  AudioSnapshot next;
  portENTER_CRITICAL(&audioMux); next = audioShared; portEXIT_CRITICAL(&audioMux);
  if (session != audioSession) {
    audioSession = session;
    ++next.generation;
    next.valid = false;
    next.gate = false;
    memset(next.material,0,sizeof(next.material));
  }
  if (valid && !next.valid) {
      uint8_t context[22];
      if (hexDecode(device+dn+hn,context,22) &&
          P8::derive(record.key,context,next.material)) {
        next.valid = true;
        memcpy(next.device,context,6);
        for (int i=0;i<4;++i) next.ip[i]=hostIP[i];
      } else Serial.println("AUDIO_DEGRADED HKDF error; muted");
  }
  next.gate = next.valid && valid && permitted && dictationReady && runtimeArmed && ptt && !submit && !comboAt && controlMode=="TALK" && millis()-lastG<1200 && millis()-lastAck<2000;
  next.permissionAt=lastG;next.ackAt=lastAck;
  if(next.epoch!=segmentSeq)audioEdgeUs=micros();
  next.epoch = segmentSeq;
  next.edgeUs = audioEdgeUs;
  portENTER_CRITICAL(&audioMux); audioShared = next; portEXIT_CRITICAL(&audioMux);
}

void audioWorker(void *) {
  I2SClass microphone;
  WiFiUDP audioUdp; // Sole owner: never shared with loop's control socket.
  microphone.setPinsPdmRx(42,41);
  microphone.setTimeout(40);
  if (!microphone.begin(I2S_MODE_PDM_RX,16000,I2S_DATA_BIT_WIDTH_16BIT,I2S_SLOT_MODE_MONO)) {
    portENTER_CRITICAL(&audioMux); ++audioMetrics.initErrors; portEXIT_CRITICAL(&audioMux);
    Serial.println("AUDIO_DEGRADED PDM init failed; controls continue");
    vTaskDelete(nullptr); return;
  }
  portENTER_CRITICAL(&audioMux); audioMetrics.ready=true; portEXIT_CRITICAL(&audioMux);
  Serial.println("AUDIO_READY PDM GPIO42/41 16k mono s16; idle samples discarded");
  uint8_t pcm[640], packet[P8::PACKET];
  uint32_t generation = 0, lastEpoch = 0;
  uint64_t sequence = 0;
  bool socketReady = false;
  for (;;) {
    AudioSnapshot before, current;
    portENTER_CRITICAL(&audioMux); before=audioShared; portEXIT_CRITICAL(&audioMux);
    uint32_t began=micros();
    size_t got = microphone.readBytes((char *)pcm,sizeof(pcm));
    portENTER_CRITICAL(&audioMux); current=audioShared; portEXIT_CRITICAL(&audioMux);
    if (got != sizeof(pcm) || micros()-began > 60000) {
      portENTER_CRITICAL(&audioMux); ++audioMetrics.readErrors; portEXIT_CRITICAL(&audioMux);
      vTaskDelay(1); continue;
    }
    if (current.generation != generation) {
      generation=current.generation; sequence=0; lastEpoch=0;
      audioUdp.stop(); socketReady=false;
    }
    if (!before.gate || !current.gate || before.generation!=current.generation || before.epoch!=current.epoch) {
      portENTER_CRITICAL(&audioMux); ++audioMetrics.discarded; portEXIT_CRITICAL(&audioMux);
      continue;
    }
    if (sequence >= P8::LIMIT) {
      portENTER_CRITICAL(&audioMux); audioMetrics.exhausted=true; portEXIT_CRITICAL(&audioMux);
      vTaskDelay(1); continue;
    }
    if (!socketReady) socketReady=audioUdp.begin(3336);
    if (!socketReady) {
      portENTER_CRITICAL(&audioMux); ++audioMetrics.sendErrors; portEXIT_CRITICAL(&audioMux);
      vTaskDelay(pdMS_TO_TICKS(20)); continue;
    }
    // Allocate once even if encryption/send fails; IV is never reused.
    if (!P8::encrypt(current.material,current.device,sequence++,current.epoch,pcm,packet)) {
      portENTER_CRITICAL(&audioMux); ++audioMetrics.cryptoErrors; portEXIT_CRITICAL(&audioMux);
      continue;
    }
    AudioSnapshot check;
    portENTER_CRITICAL(&audioMux); check=audioShared; portEXIT_CRITICAL(&audioMux);
    if (!check.gate || check.generation!=generation || check.epoch!=current.epoch || micros()-began>60000 ||
        millis()-check.permissionAt>=1200 || millis()-check.ackAt>=2000 || digitalRead(PTT)!=LOW || digitalRead(SUBMIT)==LOW) continue;
    IPAddress target(current.ip[0],current.ip[1],current.ip[2],current.ip[3]);
    bool ok=audioUdp.beginPacket(target,3333) && audioUdp.write(packet,sizeof(packet))==sizeof(packet) && audioUdp.endPacket();
    portENTER_CRITICAL(&audioMux);
    if (ok) {
      ++audioMetrics.sent;
      uint32_t elapsed=micros()-began;
      if (elapsed>audioMetrics.maxFrameUs) audioMetrics.maxFrameUs=elapsed;
      if (lastEpoch!=current.epoch) { audioMetrics.firstFrameUs=micros()-current.edgeUs; lastEpoch=current.epoch; }
    } else ++audioMetrics.sendErrors;
    portEXIT_CRITICAL(&audioMux);
    vTaskDelay(1);
  }
}

void startAudio() {
  audioCryptoPassed=audioSelfTest();
  Serial.println(audioCryptoPassed ? "AUDIO_CRYPTO_SELFTEST PASS" : "AUDIO_DEGRADED crypto selftest FAIL (muted)");
  if (!audioCryptoPassed) return;
  audioStarted=xTaskCreatePinnedToCore(audioWorker,"p8-audio",8192,nullptr,1,nullptr,0)==pdPASS;
  if (!audioStarted) Serial.println("AUDIO_DEGRADED task allocation failed; controls continue");
}

void printAudioStatus() {
  AudioMetrics m;
  portENTER_CRITICAL(&audioMux); m=audioMetrics; portEXIT_CRITICAL(&audioMux);
  Serial.printf("[AUDIO] task=%d ready=%d init_errors=%lu sent=%lu send_errors=%lu read_errors=%lu discarded=%lu crypto_errors=%lu first_frame_us=%lu max_frame_work_us=%lu\n",
    audioStarted,m.ready,(unsigned long)m.initErrors,(unsigned long)m.sent,(unsigned long)m.sendErrors,(unsigned long)m.readErrors,
    (unsigned long)m.discarded,(unsigned long)m.cryptoErrors,(unsigned long)m.firstFrameUs,(unsigned long)m.maxFrameUs);
}
