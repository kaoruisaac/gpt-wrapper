#pragma once
#include <stdint.h>
#include <string.h>
#include <mbedtls/hkdf.h>
#include <mbedtls/gcm.h>
#include <mbedtls/md.h>

// Shared by target firmware and native Mbed TLS compatibility test.
namespace P8 {
constexpr size_t HEADER = 32, PCM = 640, PACKET = HEADER + PCM + 16;
constexpr uint64_t LIMIT = 0x100000000ULL;
inline void be(uint8_t *out, uint64_t value, size_t bytes) {
  for (size_t i = 0; i < bytes; ++i) out[bytes - 1 - i] = uint8_t(value >> (8 * i));
}
inline bool derive(const uint8_t key[32], const uint8_t context[22], uint8_t out[44]) {
  const char label[] = "GPTW-P8-AUDIO-v1";
  return mbedtls_hkdf(mbedtls_md_info_from_type(MBEDTLS_MD_SHA256), context, 22,
      key, 32, (const uint8_t *)label, sizeof(label)-1, out, 44) == 0;
}
inline bool encrypt(const uint8_t material[44], const uint8_t device[6],
                    uint64_t seq, uint32_t epoch, const uint8_t pcm[PCM], uint8_t out[PACKET]) {
  if (seq >= LIMIT || !epoch || epoch == 0xffffffffUL) return false;
  memcpy(out, "P8AU", 4); out[4] = out[5] = 1;
  memcpy(out+6, device, 6); memcpy(out+12, material+36, 8);
  be(out+20, seq, 8); be(out+28, epoch, 4);
  uint8_t iv[12]; memcpy(iv, material+32, 4); be(iv+4, seq, 8);
  mbedtls_gcm_context gcm;
  mbedtls_gcm_init(&gcm);
  int rc = mbedtls_gcm_setkey(&gcm, MBEDTLS_CIPHER_ID_AES, material, 256);
  if (rc == 0) rc = mbedtls_gcm_crypt_and_tag(&gcm, MBEDTLS_GCM_ENCRYPT, PCM,
      iv, sizeof(iv), out, HEADER, pcm, out+HEADER, 16, out+HEADER+PCM);
  mbedtls_gcm_free(&gcm);
  return rc == 0;
}
}
