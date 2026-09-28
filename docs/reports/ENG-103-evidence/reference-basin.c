// Standalone Basilisk comparison; not compiled into or linked with Spall.
// Run with qcc -O2 reference-basin.c -o reference-basin -lm.
#include "grid/octree.h"
#include "embed.h"
#include "navier-stokes/centered.h"
#include "two-phase.h"
#include "reduced.h"

u.n[embed] = dirichlet(0.);
u.t[embed] = neumann(0.);
u.r[embed] = neumann(0.);
p[top] = dirichlet(0.);
pf[top] = dirichlet(0.);
u.n[top] = neumann(0.);
f[top] = dirichlet(0.);

int equilibrium = 0;

int main(int argc, char ** argv) {
  equilibrium = argc > 1;
  (void)argv;
  size(8.);
  init_grid(32); // h = 0.25 m, mask preserves Spall's actual solid faces.
  rho1 = 1000.; rho2 = 1.2;
  mu1 = mu2 = 0.;
  G.y = -9.81;
  DT = 1./60.;
  TOLERANCE = 1e-8;
  NITERMAX = 400;
  run();
}

static int fluid_space(double x, double y, double z) {
  return x >= .25 && x < 5.75 && y >= .25 && z >= .25 && z < 1.75 &&
    !(x >= 3. && x < 3.25 && y < 2.5);
}

event init(t = 0) {
  mask(y >= 3. ? top : none);
  foreach() cs[] = fluid_space(x,y,z);
  foreach_face(x) fs.x[] = fluid_space(x - Delta/2.,y,z) && fluid_space(x + Delta/2.,y,z);
  foreach_face(y) fs.y[] = fluid_space(x,y - Delta/2.,z) && fluid_space(x,y + Delta/2.,z);
  foreach_face(z) fs.z[] = fluid_space(x,y,z - Delta/2.) && fluid_space(x,y,z + Delta/2.);
  boundary({cs,fs});
  const double depth = 2.025000030175/((equilibrium ? 2.75 : 2.25)*1.5);
  foreach()
    f[] = x >= (equilibrium ? .25 : .5) && x < (equilibrium ? 3. : 2.75) && z >= .25 && z < 1.75 ?
      clamp((.25 + depth - (y - Delta/2.))/Delta, 0., 1.) : 0.;
}

event report(t = 0; t += .5; t <= 30.) {
  double water = 0., kinetic = 0., potential = 0., vmax = 0.;
  double fmin = 1., fmax = 0.;
  foreach(reduction(+:water) reduction(+:kinetic) reduction(+:potential)
          reduction(max:vmax) reduction(min:fmin) reduction(max:fmax)) {
    double v = f[]*dv(), speed2 = sq(u.x[]) + sq(u.y[]) + sq(u.z[]);
    water += v;
    kinetic += .5*rho1*v*speed2;
    potential += rho1*9.81*y*v;
    if (f[] >= 1e-3) vmax = max(vmax, sqrt(speed2));
    fmin = min(fmin, f[]); fmax = max(fmax, f[]);
  }
  double heights[54] = {0};
  foreach(serial)
    if (x >= .5 && x < 2.75 && z >= .25 && z < 1.75) {
      int ix = (int)((x - .5)/.25), iz = (int)((z - .25)/.25);
      heights[ix + 9*iz] += f[]*cs[]*Delta;
    }
  for (int a = 0; a < 54; a++)
    for (int b = a + 1; b < 54; b++)
      if (heights[a] > heights[b]) {
        double tmp = heights[a]; heights[a] = heights[b]; heights[b] = tmp;
      }
  printf("{\"t\":%.12g,\"water_m3\":%.15g,\"kinetic_j\":%.12g,"
         "\"potential_cell_center_j\":%.12g,\"eligible_max_speed_m_s\":%.12g,"
         "\"surface_column_p95_m\":%.12g,\"fraction_min\":%.12g,"
         "\"fraction_max\":%.12g,\"projection_iterations\":%d,"
         "\"projection_residual\":%.12g}\n",
         t, water, kinetic, potential, vmax, heights[50], fmin, fmax, mgp.i, mgp.resa);
  fflush(stdout);
}

event stop(t = 30.) { return 1; }
