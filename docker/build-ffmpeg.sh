#!/bin/sh
# Build a minimal, statically linked ffmpeg + ffprobe with exactly what
# subsyncer needs: every demuxer, native audio decoders, text subtitle
# decoders/encoders, raw PCM output, and the pan/aresample filters. No video
# decoding, no external libraries except zlib (compressed Matroska/MP4
# headers and tracks), no GPL components. About 19 MB instead of the ~450 MB
# a distribution ffmpeg package pulls in.
#
#   build-ffmpeg.sh <source dir> <prefix>
set -eu
src=$1
prefix=$2
cd "$src"

audio='aac,aac_fixed,aac_latm,ac3,ac3_fixed,eac3,dca,truehd,mlp,mp1,mp1float,mp2,mp2float,mp3,mp3float,mp3adu,mp3adufloat,mp3on4,mp3on4float,opus,vorbis,flac,alac,als,ape,wavpack,tta,tak,wmav1,wmav2,wmapro,wmalossless,wmavoice,cook,ra_144,ra_288,ralf,atrac1,atrac3,atrac3al,atrac3p,atrac3pal,atrac9,amrnb,amrwb,gsm,gsm_ms,qdm2,qdmc,sipr,speex,dolby_e,s302m,nellymoser,truespeech,comfortnoise,iac,imc,metasound,g723_1,g729,on2avc,binkaudio_dct,binkaudio_rdft,smackaud,xma1,xma2,osq,pcm_*,adpcm_*,dsd_*,dst,mace3,mace6,sbc,aptx,aptx_hd,ftr,hca'
subs='subrip,srt,ass,ssa,mov_text,webvtt,text,microdvd,mpl2,sami,stl,subviewer,subviewer1,realtext,pjs,vplayer,jacosub,cc_dec'

asm=''
case "$(uname -m)" in
    x86_64 | i?86) command -v nasm >/dev/null 2>&1 || asm='--disable-x86asm' ;;
esac

# shellcheck disable=SC2086
./configure --prefix="$prefix" \
    --disable-everything --disable-autodetect --disable-doc --disable-debug \
    --disable-network --disable-ffplay --disable-avdevice --disable-swscale \
    --disable-shared --enable-static --pkg-config-flags=--static --extra-ldflags=-static \
    --enable-zlib $asm \
    --enable-protocol=file,pipe \
    --enable-demuxers --enable-parsers --enable-bsfs \
    --enable-decoder="$audio,$subs" \
    --enable-encoder=pcm_s16le,subrip,srt,ass,ssa,webvtt \
    --enable-muxer=pcm_s16le,srt,ass,webvtt \
    --enable-filter=aresample,aformat,pan,anull,abuffer,abuffersink,atrim,volume
make -j"$(nproc)"
make install
strip "$prefix/bin/ffmpeg" "$prefix/bin/ffprobe"
"$prefix/bin/ffmpeg" -hide_banner -version | head -1
