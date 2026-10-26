#!/usr/bin/env python3
"""Generate a native SlideBook 7 project accepted by the Java reader.

Usage: python3 scripts/generate_slidebook7_parity_fixture.py [output-directory]
The returned .sldy path can be passed to BIOFORMATS_RS_JAVA_PARITY_EXTRA_FILES.
This synthetic fixture distinguishes every channel, Z plane and timepoint.
"""
import pathlib
import struct
import sys

root = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else '.tmp/slidebook7-parity')
group = root / 'native.dir' / 'Capture.imgdir'
group.mkdir(parents=True, exist_ok=True)
(root / 'native.sldy').write_text('SlideBook 7 synthetic parity fixture\n')
(group / 'ImageRecord.yaml').write_text('''StartClass:
  ClassName: CImageRecord70
  mWidth: 2
  mHeight: 2
  mNumPlanes: 2
  mNumChannels: 2
  mNumTimepoints: 2
  mName: Native C/Z/T
  mInfo: parity fixture
EndClass: 0
StartClass:
  ClassName: CLensDef70
  mName: 20x Air
  mMicronPerPixel: 0.5
  mActualMagnification: 20
EndClass: 0
StartClass:
  ClassName: COptovarDef70
  mMagnification: 1
EndClass: 0
''')
channels = ''
for name in ['DAPI', 'FITC']:
    channels += f'''StartClass:
  ClassName: CChannelRecord70
  mNumPlanes: 2
EndClass: 0
StartClass:
  ClassName: CExposureRecord70
  mExposureTime: 50
  mXFactor: 1
  mInterplaneSpacing: 2
EndClass: 0
StartClass:
  ClassName: CChannelDef70
  mName: {name}
EndClass: 0
StartClass:
  ClassName: CFluorDef70
  mLambda: 461
EndClass: 0
'''
(group / 'ChannelRecord.yaml').write_text(channels)
for name, text in {
    'MaskRecord.yaml': 'theNumMasks: 0\n',
    'AnnotationRecord.yaml': 'StartClass:\n  ClassName: CDataTableHeaderRecord70\nEndClass: 0\n',
    'ElapsedTimes.yaml': 'theElapsedTimes: [2, 0, 100]\n',
    'SAPositionData.yaml': 'theImageCount: 0\n',
    'StagePositionData.yaml': 'StructArraySize: 1\nStructArrayValues: [10.0, 20.0, 30.0]\n',
    'AuxData.yaml': 'theAuxFloatDataTablesSize: 0\ntheAuxDoubleDataTablesSize: 0\ntheAuxSInt32DataTablesSize: 0\ntheAuxSInt64DataTablesSize: 0\ntheAuxXmlDataTablesSize: 0\n',
}.items():
    (group / name).write_text(text)
for timepoint in range(2):
    for channel in range(2):
        payload = b''.join(struct.pack('<H', 10 + timepoint * 40 + channel * 20 + z * 10) * 4 for z in range(2))
        header = "{'descr': '<u2', 'fortran_order': False, 'shape': (2, 2, 2), }"
        header += ' ' * ((16 - (10 + len(header) + 1) % 16) % 16) + '\n'
        npy = b'\x93NUMPY\x01\x00' + struct.pack('<H', len(header)) + header.encode() + payload
        (group / f'ImageData_Ch{channel}_TP{timepoint:07}.npy').write_bytes(npy)
print((root / 'native.sldy').resolve())
