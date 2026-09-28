package dev.aas.android.ui.theme

import android.os.Build
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.ColorScheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.ReadOnlyComposable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.sp

/*
 * The app theme: Material 3 with the device's dynamic colours (Android 12+) and a teal brand
 * scheme otherwise, plus two app-specific additions every screen uses:
 *
 * * `MaterialTheme.statusColors`: the colours of the status vocabulary (ThreadActivity) and of
 *   the connection states, identical in lists, chips and banners.
 * * `MaterialTheme.codeStyle`: monospace text for commands, output and diffs.
 */

private val BrandLight = lightColorScheme(
    primary = Color(0xFF00696B),
    onPrimary = Color(0xFFFFFFFF),
    primaryContainer = Color(0xFF9CF1F2),
    onPrimaryContainer = Color(0xFF002020),
    secondary = Color(0xFF4A6363),
    onSecondary = Color(0xFFFFFFFF),
    secondaryContainer = Color(0xFFCCE8E7),
    onSecondaryContainer = Color(0xFF051F1F),
    tertiary = Color(0xFF4B607C),
    onTertiary = Color(0xFFFFFFFF),
    tertiaryContainer = Color(0xFFD3E4FF),
    onTertiaryContainer = Color(0xFF041C35),
    background = Color(0xFFF4FBFA),
    onBackground = Color(0xFF161D1D),
    surface = Color(0xFFF4FBFA),
    onSurface = Color(0xFF161D1D),
    surfaceVariant = Color(0xFFDAE5E4),
    onSurfaceVariant = Color(0xFF3F4948),
    outline = Color(0xFF6F7979),
)

private val BrandDark = darkColorScheme(
    primary = Color(0xFF80D4D6),
    onPrimary = Color(0xFF003738),
    primaryContainer = Color(0xFF004F51),
    onPrimaryContainer = Color(0xFF9CF1F2),
    secondary = Color(0xFFB0CCCB),
    onSecondary = Color(0xFF1B3434),
    secondaryContainer = Color(0xFF324B4B),
    onSecondaryContainer = Color(0xFFCCE8E7),
    tertiary = Color(0xFFB3C8E8),
    onTertiary = Color(0xFF1C314B),
    tertiaryContainer = Color(0xFF334863),
    onTertiaryContainer = Color(0xFFD3E4FF),
    background = Color(0xFF0E1515),
    onBackground = Color(0xFFDDE4E3),
    surface = Color(0xFF0E1515),
    onSurface = Color(0xFFDDE4E3),
    surfaceVariant = Color(0xFF3F4948),
    onSurfaceVariant = Color(0xFFBEC9C8),
    outline = Color(0xFF889392),
)

/** Colours of the status vocabulary and connection states. */
@Immutable
data class StatusColors(
    val running: Color,
    val needsApproval: Color,
    val needsInput: Color,
    val error: Color,
    val idle: Color,
    val unread: Color,
    val connected: Color,
    val working: Color,
    val offline: Color,
)

private val LightStatus = StatusColors(
    running = Color(0xFF1B6EF3),
    needsApproval = Color(0xFFB26A00),
    needsInput = Color(0xFF7A4FC9),
    error = Color(0xFFBA1A1A),
    idle = Color(0xFF6F7979),
    unread = Color(0xFF00696B),
    connected = Color(0xFF2E7D32),
    working = Color(0xFFB26A00),
    offline = Color(0xFF6F7979),
)

private val DarkStatus = StatusColors(
    running = Color(0xFF8AB4FF),
    needsApproval = Color(0xFFFFB95C),
    needsInput = Color(0xFFCDB5FF),
    error = Color(0xFFFFB4AB),
    idle = Color(0xFF889392),
    unread = Color(0xFF80D4D6),
    connected = Color(0xFF81C784),
    working = Color(0xFFFFB95C),
    offline = Color(0xFF889392),
)

private val LocalStatusColors = staticCompositionLocalOf { LightStatus }

private val CodeStyle = TextStyle(fontFamily = FontFamily.Monospace, fontSize = 13.sp, lineHeight = 18.sp)

private val LocalCodeStyle = staticCompositionLocalOf { CodeStyle }

/** Colours of statuses (ThreadActivity, connection) for the current light/dark mode. */
val MaterialTheme.statusColors: StatusColors
    @Composable @ReadOnlyComposable get() = LocalStatusColors.current

/** Monospace style for commands, output and diffs. */
val MaterialTheme.codeStyle: TextStyle
    @Composable @ReadOnlyComposable get() = LocalCodeStyle.current

@Composable
fun AasTheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    dynamicColor: Boolean = true,
    content: @Composable () -> Unit,
) {
    val context = LocalContext.current
    val scheme: ColorScheme = when {
        dynamicColor && Build.VERSION.SDK_INT >= Build.VERSION_CODES.S ->
            if (darkTheme) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
        darkTheme -> BrandDark
        else -> BrandLight
    }
    CompositionLocalProvider(
        LocalStatusColors provides if (darkTheme) DarkStatus else LightStatus,
        LocalCodeStyle provides CodeStyle,
    ) {
        MaterialTheme(colorScheme = scheme, typography = Typography(), content = content)
    }
}
