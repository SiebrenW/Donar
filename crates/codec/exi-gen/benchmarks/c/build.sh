#!/bin/sh
# Builds the C harnesses into ../build: `speed` (-O2, all protocols) and one `size` binary per
# protocol (-Os, dead code stripped) plus an empty baseline. Needs data/bench/{OpenV2G,libcbv2g}
# (git clone https://github.com/Martin-P/OpenV2G, https://github.com/EVerest/libcbv2g).
set -e
here=$(cd "$(dirname "$0")" && pwd)
bench=$here/../../../../../data/bench
out=$here/../build
mkdir -p "$out"
# OpenV2G's codecs are off by default in the headers: switch on DIN (and XML signatures for ISO 15118-2).
rm -rf "$out/openv2g-src" && cp -R "$bench/OpenV2G/src" "$out/openv2g-src"
sed -i.bak 's/define DEPLOY_DIN_CODEC SUPPORT_NO/define DEPLOY_DIN_CODEC SUPPORT_YES/; s/define DEPLOY_DIN_CODEC_FRAGMENT SUPPORT_NO/define DEPLOY_DIN_CODEC_FRAGMENT SUPPORT_YES/' "$out/openv2g-src/din/dinEXIDatatypes.h"
sed -i.bak 's/define DEPLOY_XMLDSIG_CODEC SUPPORT_NO/define DEPLOY_XMLDSIG_CODEC SUPPORT_YES/; s/define DEPLOY_XMLDSIG_CODEC_FRAGMENT SUPPORT_NO/define DEPLOY_XMLDSIG_CODEC_FRAGMENT SUPPORT_YES/' "$out/openv2g-src/xmldsig/xmldsigEXIDatatypes.h"
ov=$out/openv2g-src
OV_INC="-I$here -I$ov -I$ov/codec -I$ov/din -I$ov/iso1 -I$ov/xmldsig"
OV_CODEC="$ov/codec/*.c"
CB=$bench/libcbv2g
CB_INC="-I$here -I$CB/include"
CB_COMMON="$CB/lib/cbv2g/common/*.c"
cc=${CC:-cc}
# speed
$cc -O2 $OV_INC -DP_DIN -DP_ISO2 "$here/openv2g.c" $OV_CODEC "$ov/din/"*.c "$ov/iso1/"*.c "$ov/xmldsig/"*.c -o "$out/openv2g"
$cc -O2 $CB_INC -DP_DIN -DP_ISO2 -DP_ISO20 "$here/cbv2g.c" $CB_COMMON "$CB/lib/cbv2g/din/"*.c "$CB/lib/cbv2g/iso_2/"*.c "$CB/lib/cbv2g/iso_20/iso20_CommonMessages_"*.c "$CB/lib/cbv2g/iso_20/iso20_DC_"*.c "$CB/lib/cbv2g/iso_20/iso20_AC_"*.c -o "$out/cbv2g"
# size: -Os, dead code stripped (the linker flag differs between macOS and Linux)
if [ "$(uname)" = Darwin ]; then strip_flag=-Wl,-dead_strip; else strip_flag=-Wl,--gc-sections; fi
$cc -Os -ffunction-sections -fdata-sections $strip_flag $OV_INC "$here/openv2g.c" $OV_CODEC -o "$out/size-openv2g-none"
$cc -Os -ffunction-sections -fdata-sections $strip_flag $OV_INC -DP_DIN "$here/openv2g.c" $OV_CODEC "$ov/din/"*.c -o "$out/size-openv2g-din"
$cc -Os -ffunction-sections -fdata-sections $strip_flag $OV_INC -DP_ISO2 "$here/openv2g.c" $OV_CODEC "$ov/iso1/"*.c "$ov/xmldsig/"*.c -o "$out/size-openv2g-iso2"
$cc -Os -ffunction-sections -fdata-sections $strip_flag $CB_INC "$here/cbv2g.c" $CB_COMMON -o "$out/size-cbv2g-none"
$cc -Os -ffunction-sections -fdata-sections $strip_flag $CB_INC -DP_DIN "$here/cbv2g.c" $CB_COMMON "$CB/lib/cbv2g/din/"*.c -o "$out/size-cbv2g-din"
$cc -Os -ffunction-sections -fdata-sections $strip_flag $CB_INC -DP_ISO2 "$here/cbv2g.c" $CB_COMMON "$CB/lib/cbv2g/iso_2/"*.c -o "$out/size-cbv2g-iso2"
$cc -Os -ffunction-sections -fdata-sections $strip_flag $CB_INC -DP_ISO20 "$here/cbv2g.c" $CB_COMMON "$CB/lib/cbv2g/iso_20/iso20_CommonMessages_"*.c "$CB/lib/cbv2g/iso_20/iso20_DC_"*.c "$CB/lib/cbv2g/iso_20/iso20_AC_"*.c -o "$out/size-cbv2g-iso20"
