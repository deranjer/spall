from fractions import Fraction as Q
import struct, math
from pathlib import Path

def f32(x): return struct.unpack('<f', struct.pack('<f', x))[0]
def bits(x): return struct.unpack('<I', struct.pack('<f', x))[0]
def normalized(z,w):
    z,w=f32(z),f32(w)
    n=f32(math.sqrt(f32(f32(z*z)+f32(w*w))))
    return Q(f32(z/n)),Q(f32(w/n))
def corners(z,w,dx,dy):
    z,w=normalized(z,w)
    # Exact affine image of all four corners, no support-radius calculation.
    a,b=1-2*z*z,2*z*w
    return [(Q(dx)+a*x-b*y,Q(dy)+b*x+a*y) for x,y in [(0,0),(1,0),(1,1),(0,1)]]
def classify(a,b):
    boundary=False
    for poly in [a,b]:
        for i in range(2):
            edge=(poly[i+1][0]-poly[i][0],poly[i+1][1]-poly[i][1])
            axis=(-edge[1],edge[0])
            pa=[axis[0]*x+axis[1]*y for x,y in a]
            pb=[axis[0]*x+axis[1]*y for x,y in b]
            gap=min(max(pa),max(pb))-max(min(pa),min(pb))
            if gap<0:return 2
            if gap==0:boundary=True
    return 0 if boundary else 1

quats=[(0.6,0.8),(0.8,0.6),(-0.6,-0.8),(0.000001,1.0),(0.0,1.0)]
offsets=[-2.0,-1.25,-0.75,0.0,0.25,0.75,1.25,2.0]
rows=[]
for i,qa in enumerate(quats):
    for qb in quats:
        for j,x in enumerate(offsets):
            for y in offsets:
                expected=classify(corners(*qa,0.0,0.0),corners(*qb,x,y))
                if expected:
                    rows.append(([bits(v) for v in (*qa,*qb,x,y)],expected))

header='''//! Independent exact-rational projected-corner oracle, generated offline with
//! Python fractions.Fraction from canonical binary32 inputs. No interval or
//! support-radius implementation is reused. Boundary contacts are excluded.
use spall_core::{GlobalCell, MaterialId, VolumeId};
use spall_physics::{OccupancyGrid,PartTransform,SourcePart,prepare_fracture};
use spall_physics::fracture::{FractureError,FracturePartSnapshot,FractureSnapshot,FractureSnapshotToken};

#[test]
fn rational_corner_oracle_rejects_false_welds_and_false_separation() {
    let mut classified=0;
    let mut ambiguous=0;
    for &(bits,expected) in CASES {
        let v=bits.map(f32::from_bits);
        let parts=(0..2).map(|i|FracturePartSnapshot {
            source:SourcePart { part_id:i as u32+1,parent_id:None,volume_id:VolumeId::new(i as u64+8100).unwrap(),source_revision:1,
                grid:OccupancyGrid::from_solid_mask(GlobalCell::new(0,0,0),[1,1,1],vec![true],vec![MaterialId(1)]).unwrap(),
                cell_m:1.0,local_transform:PartTransform::IDENTITY },
            root_local_pose:PartTransform { translation_m:if i==0 {[0.0;3]} else {[v[4],v[5],0.0]},rotation_xyzw:[0.0,0.0,v[i*2],v[i*2+1]] }
        }).collect();
        let snapshot=FractureSnapshot {token:FractureSnapshotToken {corpse_key:8100,component_key:None,topology_revision:1,pose_epoch:1,source_generation:1},parts};
        match prepare_fracture(&snapshot) {
            Ok(plan)=> {assert_eq!(plan.components.len(),usize::from(expected),"input {bits:?}");classified+=1;}
            Err(FractureError::AmbiguousContact{..})=>ambiguous+=1,
            Err(other)=>panic!("unexpected {other:?} for {bits:?}"),
        }
    }
    assert!(classified>CASES.len()/2,"insufficient classified coverage: {classified}");
    eprintln!("exact-rational corner oracle: {classified} classified, {ambiguous} conservative ambiguities, {} total",CASES.len());
}

#[rustfmt::skip]
const CASES:&[([u32;6],u8)]=&[
'''
tail='];\n'
data=''.join('    (['+','.join(map(str,b))+'],'+str(e)+'),\n' for b,e in rows)
Path('crates/spall_physics/tests/fracture_exact_oracle.rs').write_text(header+data+tail,encoding='utf-8')
print(f'Generated {len(rows)} nonboundary exact-rational oracle cases')
